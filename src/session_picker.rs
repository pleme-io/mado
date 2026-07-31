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

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use ishou_tokens::{FleetSessionNames, SessionName};
use tear_types::{DefinitionId, MultiplexerControl, PaneId, SessionId, SessionSource};

use crate::suggest::{StoredSuggestion, SuggestionId, SuggestionStore};

// The picker's visible row count is no longer a fixed constant: the renderer
// derives it from the live surface height as a typed `crate::row_budget::
// VisibleRows` (`overlay_row_budget`), so the Ctrl-S board reflows with the
// window. Only the reserved-band ANCHOR (a screen-less union-ordering policy)
// keeps a constant, `crate::row_budget::ROWS_DEFAULT`.

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
    /// The suggestion's urgency, carried ON the row so the renderer tints
    /// without touching the store (the frame loop stays lock-free).
    /// `None` for session / preset / create rows — calm default colour.
    pub urgency: Option<crate::suggest::Urgency>,
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

    /// Rename a live session to `new_name` — the Ctrl-E inline-rename commit.
    /// The name flows to the tear session OWNER (the PTY-owning `s.name` via
    /// `MultiplexerControl::rename_session`) AND the praça `custom_name` (so
    /// `display_name()` + the fuzzy index reflect it). An empty `new_name`
    /// clears `custom_name`, reverting to the emoji/glyph identity. Default
    /// `false` (an inert bridge doesn't rename); the live praça bridge
    /// overrides it.
    fn rename_session(&self, _session: SessionId, _new_name: &str, _now: u64) -> bool {
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

    /// Subscribe to the suggestion store's change broadcast (stage 3 of the
    /// live-stream substrate). The input engine holds the returned receiver and
    /// re-lists the open Ctrl-S board the moment a source watcher writes fresh
    /// rows — an event-driven subscription, never a fixed poll. `None` when the
    /// suggestion stream is off (or a test bridge without a store), in which
    /// case the board simply shows no live suggestions.
    fn suggestion_subscribe(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
    }

    /// One dim footer line summarizing lanes that are not reporting, split
    /// by VERDICT: blind lanes (never once succeeded) are NAMED, because
    /// each one is a declaration to go fix; degraded lanes (worked before,
    /// failing now) are a bare COUNT, because they are weather. A board
    /// that cannot see then reads as blind instead of calm — and "blind"
    /// keeps meaning blind. `None` = all healthy (or no store). Read at
    /// `list()` time (the store lock is legal here, never in the frame
    /// loop); the engine carries it on the picker state.
    fn health_footer(&self) -> Option<String> {
        None
    }
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
    /// Band rows guaranteed INSIDE the picker window on the empty query
    /// (`suggestions.reserved_rows`): the band is inserted at
    /// `ROWS_DEFAULT - reserve` (the reserved anchor) instead of appended, so a long session list
    /// can't push every suggestion below the fold. 0 = plain append.
    suggest_reserved: usize,
    /// Memoized ranked snapshot, keyed by the store's change-`generation`. The
    /// expensive clone-whole-map + sort is skipped while the watchers idle (the
    /// generation hasn't moved); the cheap query-filter + live-dedup + balance
    /// still run each call. `RefCell` → mutable from the `&self` list path.
    ranked_cache: RefCell<Option<(u64, Arc<Vec<StoredSuggestion>>)>>,
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
        suggest_reserved: usize,
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
            suggest_reserved,
            ranked_cache: RefCell::new(None),
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
                // `&[]`: the picker spawns a bare interactive login shell —
                // there is no per-session argv here. The operator's
                // `shell.args` knob is still unwired everywhere (see this
                // repo's CLAUDE.md `pending-tear-bump:` note); wiring it is a
                // decision about the local-PTY path too, not an insertion.
                &[],
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
        let q = query.trim();
        let now_ms = now.saturating_mul(1000);
        // Over-fetch then query-filter so a needle still finds lower-ranked
        // suggestions (the store is already living-board ranked: urgency →
        // score+aging+recurrence). `ranked_stored` carries each row's birth
        // time, recurrence count, and lifecycle state.
        // Memoized ranked snapshot — recompute the clone-whole-map + sort only
        // when the store's change-generation advanced OR the 30s aging bucket
        // rolled (the living-board rank drifts with wall time even on a silent
        // store); reuse it across the rapid re-list calls (open + every
        // keystroke + the event-driven store-broadcast re-list) while the
        // watchers idle. The query-filter + live-dedup below still run each call.
        let memo_key = store.generation() ^ (now_ms / 30_000).rotate_left(32);
        let ranked: Arc<Vec<crate::suggest::StoredSuggestion>> = {
            let mut cache = self.ranked_cache.borrow_mut();
            match cache.as_ref() {
                Some((g, v)) if *g == memo_key => Arc::clone(v),
                _ => {
                    let fresh = Arc::new(store.ranked_stored(
                        self.suggest_max.saturating_mul(4).max(self.suggest_max),
                        now_ms,
                    ));
                    *cache = Some((memo_key, Arc::clone(&fresh)));
                    fresh
                }
            }
        };
        // A suggestion you've already STARTED is a live ● session row now —
        // suppress its ○ twin so the picker never lists the same task twice.
        // The suppressed rows' correlation keys are COLLECTED: the same task
        // spelled by another source (🎫 vs 📋 the same ticket) must not
        // resurface as latent while its session is live.
        let mut live_corrs: std::collections::HashSet<crate::suggest::CorrKey> =
            std::collections::HashSet::new();
        let mut live_or_collect = |st: &crate::suggest::StoredSuggestion| -> bool {
            let live = self.live_session_for(&st.item.spawn).is_some();
            if live {
                if let Some(c) = st.item.corr.clone() {
                    live_corrs.insert(c);
                }
            }
            live
        };
        let filtered: Vec<crate::suggest::StoredSuggestion> = if q.is_empty() {
            // Empty query → keep the store's urgency/score order.
            ranked
                .iter()
                .filter(|st| !live_or_collect(st))
                .cloned()
                .collect()
        } else {
            // Non-empty query → FUZZY-rank with the SAME praça matcher the
            // session/preset rows use, so Ctrl-S matching is ONE algorithm
            // across sessions, presets, AND suggestions. Best field score wins;
            // ties fall back to the store's urgency/score order (the over-fetch
            // index `idx`), so the strongest textual match floats to the top.
            let mut scored: Vec<(i32, usize, crate::suggest::StoredSuggestion)> = ranked
                .iter()
                .enumerate()
                .filter(|(_, st)| !live_or_collect(st))
                .filter_map(|(idx, st)| {
                    suggestion_match_score(q, &st.item).map(|sc| (sc, idx, st.clone()))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            scored.into_iter().map(|(_, _, st)| st).collect()
        };
        // Collapse cross-source twins BEFORE the per-source balance, so an
        // absorbed duplicate never consumes a cap slot or a max_visible slot
        // — the band stays full of DISTINCT work.
        let filtered = crate::suggest::store::collapse_correlated(filtered, &live_corrs);
        // Balance the band: cap per-source so one noisy source (20 CrashLoop
        // pods) can't drown your PRs/tickets/incidents — the band stays diverse.
        crate::suggest::store::balance_per_source(filtered, self.suggest_max, self.suggest_cap)
            .into_iter()
            .map(|st| {
                // ◐ in progress (accepted, session open) vs ○ latent — the
                // board legibly separates "waiting" from "being worked".
                let mut label = String::from(
                    if matches!(st.state, crate::suggest::SuggestionState::Accepted { .. }) {
                        "\u{25d0} " // ◐ in progress
                    } else {
                        "\u{25cb} " // ○ latent
                    },
                );
                label.push_str(&st.item.picker_label().to_string());
                // Recurrence stamp: a repeat offender wears its count (×3), so
                // a flapping alert reads differently from a first-timer.
                if st.times_seen >= 2 {
                    label.push_str("  \u{d7}");
                    label.push_str(&st.times_seen.to_string());
                }
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
                    kind: RowKind::Suggestion(st.item.id),
                    urgency: Some(st.item.urgency),
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
            let ok = self.switch_to(existing);
            if ok {
                store.mark_accepted(id, sug.spawn.name());
            }
            return ok;
        }
        // The full ordered prewarm strategy for this suggestion (legacy
        // `initial_command` + the richer `with_prewarm` steps). Computed BEFORE
        // spawn so the pre-spawn `SetEnv` steps can be folded into the child's
        // env — env can't be cleanly injected into an already-live shell.
        let prewarm = sug.spawn.prewarm();

        // Spawn into the suggestion's cwd (mado's capability env, cwd
        // overridden), with the prewarm's SetEnv steps folded onto the env
        // overrides (pre-spawn half of the strategy).
        let cwd = sug.spawn.cwd().to_string_lossy().into_owned();
        let mut env = self.spawn_env_base.clone().with_cwd(Some(cwd));
        for (k, v) in prewarm.env_steps() {
            env.overrides.push((k.to_string(), v.to_string()));
        }
        self.inproc.set_spawn_env(env);
        let Ok(sid) = self.inproc.new_session_with_source_and_size(
            sug.spawn.name(),
            &self.shell,
            // `&[]` deliberately: a suggestion's payload is NOT argv. It is a
            // prewarm program — `SetEnv` steps folded into the spawn env above,
            // then `RunCommand`/`KubeContext` steps delivered as KEYSTROKES to
            // the live interactive shell below. Passing them as argv would
            // replace the shell instead of driving it.
            &[],
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
        // The magic: prewarm the fresh session. Enter on a
        // "🔍 pr#1234 fix the parser" suggestion lands you in the repo AND runs
        // `gh pr checkout 1234`; Enter on an alert suggestion lands you already
        // in the issue (kube-context set, pod described, runbook open).
        // `spawn.prewarm()` is the full ordered strategy: the legacy
        // `initial_command` folded in as the first `RunCommand`, then the richer
        // steps the safra builder attached via `with_prewarm`. SetEnv steps were
        // already folded into `spawn_env_base` pre-spawn above; `apply` runs the
        // post-spawn steps in order through the typed `PrewarmEnv` seam. No-op
        // for a bare suggestion.
        if !prewarm.is_empty() {
            let mut penv = SessionPrewarmEnv { inproc: self.inproc.as_ref(), pane };
            let _ = crate::prewarm::apply(&prewarm, &mut penv);
        }
        // Soft-ack: the row is now IN PROGRESS — demoted below fresh work and
        // badged ◐, but still on the board until upstream confirms resolution
        // (decay) or the operator dismisses it.
        store.mark_accepted(id, sug.spawn.name());
        self.switch.post(pane);
        true
    }
}

/// Best fuzzy score of `query` against any matchable field of a suggestion —
/// the displayed title, its detail line, the spawn (session) name, the spawn
/// cwd (so typing a repo name finds its suggestions, mirroring the session
/// matcher's path tier), and the source's slug + human label (so typing `jira`
/// matches every Jira row, `pr` every PR row). Uses the SAME
/// `praca::index::fuzzy_score` the session/preset rows rank through, so Ctrl-S
/// matching is one algorithm across every row kind. `None` if nothing matches.
fn suggestion_match_score(query: &str, s: &crate::suggest::Suggestion) -> Option<i32> {
    use praca::index::fuzzy_score;
    let mut best: Option<i32> = None;
    let mut consider = |hay: &str| {
        if let Some(sc) = fuzzy_score(query, hay) {
            best = Some(best.map_or(sc, |b: i32| b.max(sc)));
        }
    };
    consider(s.title.as_str());
    if let Some(d) = s.detail.as_deref() {
        consider(d);
    }
    consider(s.spawn.name());
    consider(&s.spawn.cwd().to_string_lossy());
    consider(s.source.slug());
    consider(s.source.label());
    best
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

/// The real [`crate::prewarm::PrewarmEnv`] executor — actuates a prewarm
/// strategy's post-spawn steps against a freshly-spawned session. Reuses the
/// shipped, no-shell verbs: keystrokes through the typed `MultiplexerControl`
/// seam (as the single kickoff always did) and links through mado's typed URL
/// opener (`/usr/bin/open` / `xdg-open`, never a shell). Keeps the scope
/// boundary: the strategy is data, this executor runs it in the new session's
/// shell — no privileged write-intrinsics.
struct SessionPrewarmEnv<'a> {
    inproc: &'a tear_core::InProcess,
    pane: PaneId,
}

impl crate::prewarm::PrewarmEnv for SessionPrewarmEnv<'_> {
    fn run_command(&mut self, cmd: &str) {
        // PTY input buffering carries the keystrokes until the shell is ready,
        // so this works exactly like typing-ahead (as the kickoff did).
        let _ = self.inproc.send_keys(self.pane, &kickoff_keystrokes(cmd));
    }

    fn open_url(&mut self, url: &url::Url) {
        // Detached, never blocks; log-and-continue on error (a failed dashboard
        // open must not abort the rest of the prewarm).
        if let Err(e) = crate::url::open_link(url.as_str()) {
            tracing::warn!(error = %e, url = %url, "prewarm: open_url failed");
        }
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
                        urgency: None,
                    },
                    praca::Ranked::Latent(def) => SessionPickerRow {
                        label: Self::row_label(
                            &def.display_name(),
                            &def.project_root,
                            badge_rows.then_some(true),
                        ),
                        kind: RowKind::Instantiate(def.def_id),
                        urgency: None,
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
        //
        // Reserved band quota (empty query only): a long session list would
        // otherwise push the whole band below the render fold. INSERT the
        // band at `ROWS_DEFAULT - reserve` (the reserved anchor) — displaced union rows slide below
        // it (still scrollable, still counted by the +N-more footer), and
        // nothing is ever truncated. Non-empty query: plain append (fuzzy
        // rank rules the whole list).
        let band = self.suggestion_rows(query, now);
        if query.trim().is_empty() && !band.is_empty() && self.suggest_reserved > 0 {
            // Reserved-band anchor: a union-ordering policy (where the
            // suggestion band lands within the first rows), NOT a viewport-fit
            // concern — this seam has no screen access. The VISIBLE row cap is
            // screen-derived at the render layer (`overlay_row_budget`); this
            // anchor stays the historical constant so the band never sinks
            // below the guaranteed-visible head of the list.
            let anchor = crate::row_budget::ROWS_DEFAULT;
            let reserve = self.suggest_reserved.min(band.len()).min(anchor - 1);
            let cut = rows.len().min(anchor - reserve);
            let tail = rows.split_off(cut);
            rows.extend(band);
            rows.extend(tail);
        } else {
            rows.extend(band);
        }
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
                    urgency: None,
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
            urgency: None,
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

    fn rename_session(&self, session: SessionId, new_name: &str, now: u64) -> bool {
        // (a) Rename the PTY-owning tear session — the MultiplexerControl RPC
        // on the embedded InProcess owner; this IS "all the way down to tear".
        let tear_ok = self.inproc.rename_session(session, new_name).is_ok();
        // (b) Mirror into the praça custom_name so display_name() + the fuzzy
        // index reflect it immediately (an empty name clears custom_name,
        // reverting to the emoji/glyph identity — a free "reset name").
        let praca_ok = {
            let mut praca = self.praca();
            match praca.index.get_mut(session) {
                Some(rec) => {
                    rec.rename(new_name);
                    true
                }
                None => false,
            }
        };
        // Keep the just-renamed session MRU-fresh (same rationale as switch_to).
        if tear_ok || praca_ok {
            self.praca().record_visit(session, now);
        }
        tear_ok || praca_ok
    }

    fn refresh(&self, now: u64) {
        use crate::picker::reconcile::IndexReconciler;
        let reconciler =
            crate::picker::reconcile::InProcessSessionReconciler::new(Arc::clone(&self.inproc));
        let mut praca = self.praca();
        reconciler.reconcile(&mut praca, now);
    }

    fn suggestion_subscribe(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.suggestions.as_ref().map(|store| store.subscribe())
    }

    fn health_footer(&self) -> Option<String> {
        let store = self.suggestions.as_ref()?;
        if self.suggest_max == 0 {
            return None;
        }
        // Split the non-Ok lanes by VERDICT, not by status. This line used
        // to call EVERY non-Ok lane "blind", which spent the one word that
        // should mean "has never worked" on lanes that were merely having a
        // bad minute — so when a genuinely blind lane appeared, the footer
        // had no stronger thing left to say. Only armed sources ever record
        // a poll; BTreeMap keeps this catalog-ordered and stable.
        let mut blind: Vec<(crate::suggest::SourceKind, crate::suggest::SourceHealth)> = Vec::new();
        let mut degraded = 0usize;
        for (kind, h) in store.health() {
            match crate::suggest::verdict(&h) {
                crate::suggest::HealthVerdict::Ok => {}
                crate::suggest::HealthVerdict::Degraded => degraded += 1,
                crate::suggest::HealthVerdict::Blind => blind.push((kind, h)),
            }
        }
        if blind.is_empty() && degraded == 0 {
            return None;
        }
        let mut line = String::from("\u{26a0} "); // warning sign
        if !blind.is_empty() {
            line.push_str(&blind.len().to_string());
            line.push_str(if blind.len() == 1 {
                " lane blind (never once succeeded): "
            } else {
                " lanes blind (never once succeeded): "
            });
            // Blind lanes are named individually — they are the actionable
            // ones, and the name IS the thing to go fix.
            for (i, (kind, health)) in blind.iter().take(3).enumerate() {
                if i > 0 {
                    line.push_str(" \u{00B7} "); // middle dot
                }
                line.push_str(kind.slug());
                line.push(' ');
                line.push_str(health.status.label());
            }
            if blind.len() > 3 {
                line.push_str(" +");
                line.push_str(&(blind.len() - 3).to_string());
            }
        }
        if degraded > 0 {
            if !blind.is_empty() {
                line.push_str(" \u{00B7} "); // middle dot
            }
            // Degraded lanes get a bare COUNT: they have worked before and
            // are expected to work again, so naming each one every frame is
            // noise that would crowd out the names that matter.
            line.push_str(&degraded.to_string());
            line.push_str(" degraded");
        }
        Some(line)
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
                &[],
                SessionSource::Named("test".into()),
                (80, 24),
            )
            .expect("spawn live session");
        (inproc, live)
    }

    #[test]
    fn bridge_lists_live_sessions_and_switch_posts_pane() {
        let (inproc, live) = live_inproc();
        // The expect IS the assertion (a live session must expose a pane).
        let _live_pane = inproc
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
        let spawn = |name: &str| {
            inproc
                .new_session_with_source_and_size(
                    name,
                    "/bin/sh",
                    &[],
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
    fn suggestion_match_score_is_fuzzy_and_field_wide() {
        let s = sug(
            crate::suggest::SourceKind::JiraSprint,
            "PLEME-42",
            "PLEME-42 fix the parser",
            "/code/pleme-io/mado",
        );
        // Subsequence match on the title (the praça matcher, not substring).
        assert!(suggestion_match_score("parser", &s).is_some());
        // The source slug ("jira-sprint") is matchable — typing the source
        // name surfaces every row of that source.
        assert!(suggestion_match_score("jira", &s).is_some());
        // The spawn (session) name is matchable too.
        assert!(suggestion_match_score("mado", &s).is_some());
        // A query that hits no field scores nothing (the row is filtered out).
        assert!(suggestion_match_score("zzqqxx", &s).is_none());
    }

    #[test]
    fn suggestion_rows_rank_high_priority_first_on_empty_query() {
        use crate::suggest::{SourceKind, Urgency};
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        // A high-priority (Critical) Jira ticket + an ordinary github issue.
        let hi = sug(SourceKind::JiraAssigned, "PLEME-1", "PLEME-1 urgent fix", "/code/a")
            .urgent(Urgency::Critical)
            .scored(1000);
        let lo = sug(SourceKind::GithubAssignedIssues, "gh-2", "gh issue", "/code/b");
        store.ingest(SourceKind::JiraAssigned, vec![hi], 1000);
        store.ingest(SourceKind::GithubAssignedIssues, vec![lo], 1000);
        let bridge = bridge_with_suggestions(praca::Praca::new(), inproc, store, 6);
        // Empty query → urgency order → the high-priority Jira ticket is first
        // (the "bring high priority jira to the absolute top" behavior).
        let rows = bridge.suggestion_rows("", 1000);
        assert!(
            rows.first().is_some_and(|r| r.label.contains("PLEME-1")),
            "high-priority Jira ranks first: {:?}",
            rows.iter().map(|r| &r.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn suggestion_rows_rank_by_match_quality_on_query() {
        use crate::suggest::SourceKind;
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        let a = sug(SourceKind::TendRepos, "a", "deploy the parser", "/code/a");
        let b = sug(SourceKind::TendRepos, "b", "bump deps", "/code/b");
        store.ingest(SourceKind::TendRepos, vec![a, b], 1000);
        let bridge = bridge_with_suggestions(praca::Praca::new(), inproc, store, 6);
        // A typed query ranks by match quality; the non-match is filtered out.
        let rows = bridge.suggestion_rows("parser", 1000);
        assert!(rows.first().is_some_and(|r| r.label.contains("parser")));
        assert!(!rows.iter().any(|r| r.label.contains("bump deps")));
    }

    #[test]
    fn suggestion_rows_multi_term_query_is_and() {
        use crate::suggest::SourceKind;
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        let a = sug(SourceKind::GitBranchPr, "pr7", "pr#7 fix mado", "/code/mado");
        let b = sug(SourceKind::GitBranchPr, "pr8", "pr#8 fix tear", "/code/tear");
        store.ingest(SourceKind::GitBranchPr, vec![a, b], 1000);
        let bridge = bridge_with_suggestions(praca::Praca::new(), inproc, store, 6);
        // "mado pr" — both terms must match; only the mado PR carries both.
        let rows = bridge.suggestion_rows("mado pr", 1000);
        assert_eq!(rows.len(), 1, "{:?}", rows.iter().map(|r| &r.label).collect::<Vec<_>>());
        assert!(rows[0].label.contains("mado"));
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

    /// The reserved band quota: with more sessions than the render window,
    /// the empty-query band is INSERTED above the fold (displaced sessions
    /// slide below, nothing truncated); a query keeps the plain append.
    #[test]
    fn reserved_band_rows_stay_inside_the_window_on_the_empty_query() {
        let inproc = Arc::new(tear_core::InProcess::new());
        inproc.set_spawn_env(tear_types::SpawnEnv::none());
        let mut praca = praca::Praca::new();
        let now = 1000u64;
        // 14 live sessions — enough to bury an appended band below row 12.
        for i in 0..14 {
            let mut name = String::from("sess");
            name.push_str(&i.to_string());
            let sid = inproc
                .new_session_with_source_and_size(
                    &name,
                    "/bin/sh",
                    &[],
                    SessionSource::Named("test".into()),
                    (80, 24),
                )
                .expect("spawn");
            let mut root = String::from("/code/p/");
            root.push_str(&name);
            praca
                .index
                .upsert(praca::SessionRecord::for_project(sid, PathBuf::from(root), SessionNameStyle::Emoji, now));
        }
        let store = Arc::new(SuggestionStore::new());
        let items: Vec<crate::suggest::Suggestion> = (0..4)
            .map(|i| {
                let mut k = String::from("t");
                k.push_str(&i.to_string());
                sug(crate::suggest::SourceKind::JiraAssigned, &k, &k, "/code/x")
            })
            .collect();
        store.ingest(crate::suggest::SourceKind::JiraAssigned, items, now * 1000);

        let switch = crate::session_switch::SwitchRequests::default();
        switch.attach_sink();
        let bridge = PracaPickerBridge::new(
            Arc::new(Mutex::new(praca)),
            Arc::clone(&inproc),
            switch,
            "/bin/sh".to_owned(),
            tear_types::SpawnEnv::none(),
            true,
            crate::config::BadgeMode::Auto,
            Some(store),
            6,
            0,
            3, // reserved_rows
        );

        // Empty query: the band is inserted at ROWS_DEFAULT - 3 = row 9, so
        // rows 9..12 (inside the reserved anchor) are suggestions and the
        // displaced sessions follow after the band. Nothing is lost.
        let win = crate::row_budget::ROWS_DEFAULT;
        let rows = bridge.list("", now);
        assert_eq!(rows.len(), 14 + 4, "insert, never truncate");
        for idx in (win - 3)..win {
            assert!(
                matches!(rows[idx].kind, RowKind::Suggestion(_)),
                "row {idx} inside the window is a band row"
            );
        }
        assert!(
            matches!(rows[win - 4].kind, RowKind::Switch(_)),
            "the row above the band is still a session"
        );
        assert!(
            matches!(rows[win + 1].kind, RowKind::Switch(_)),
            "displaced sessions slide below the band, still reachable"
        );

        // Non-empty query: plain append (fuzzy rank rules) — band after
        // every matching session row.
        let rows = bridge.list("sess", now);
        let last_switch = rows.iter().rposition(|r| matches!(r.kind, RowKind::Switch(_))).unwrap();
        let first_sug = rows.iter().position(|r| matches!(r.kind, RowKind::Suggestion(_)));
        if let Some(fs) = first_sug {
            assert!(last_switch < fs, "with a query the band appends below");
        }
    }

    /// The health footer names blind lanes; all-Ok (or no store) is None.
    #[test]
    fn health_footer_names_blind_lanes_and_stays_quiet_when_healthy() {
        let (inproc, live) = live_inproc();
        let mut praca = praca::Praca::new();
        praca.index.upsert(praca::SessionRecord::for_project(
            live,
            PathBuf::from("/code/pleme-io/mado"),
            SessionNameStyle::Emoji,
            1000,
        ));
        let store = Arc::new(SuggestionStore::new());
        let bridge = bridge_with_suggestions(praca, inproc, Arc::clone(&store), 6);
        assert!(bridge.health_footer().is_none(), "no polls yet → quiet");

        store.record_poll(crate::suggest::SourceKind::TendRepos, crate::suggest::SourceStatus::Ok, 1000);
        assert!(bridge.health_footer().is_none(), "all-Ok → quiet");

        store.record_poll(
            crate::suggest::SourceKind::GrafanaAlerts,
            crate::suggest::SourceStatus::Error,
            2000,
        );
        store.record_poll(
            crate::suggest::SourceKind::DatadogMonitors,
            crate::suggest::SourceStatus::AuthMissing,
            2000,
        );
        let footer = bridge.health_footer().expect("blind lanes surface");
        assert!(footer.contains("2 lanes blind"), "{footer}");
        assert!(footer.contains("grafana-alerts erroring"), "{footer}");
        assert!(footer.contains("datadog-monitors needs auth"), "{footer}");
    }

    /// The footer must spend the word "blind" only on lanes that have never
    /// once succeeded. A lane that worked and is failing now is weather: it
    /// contributes a bare count, never a name, so the names on the line are
    /// always the ones worth acting on.
    #[test]
    fn health_footer_separates_blind_lanes_from_merely_degraded_ones() {
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        let bridge = bridge_with_suggestions(praca::Praca::new(), inproc, Arc::clone(&store), 6);

        // jira-sprint succeeds once, THEN starts failing → degraded.
        store.record_poll(
            crate::suggest::SourceKind::JiraSprint,
            crate::suggest::SourceStatus::Ok,
            1000,
        );
        store.record_poll(
            crate::suggest::SourceKind::JiraSprint,
            crate::suggest::SourceStatus::Error,
            2000,
        );
        let footer = bridge.health_footer().expect("a degraded lane still surfaces");
        assert!(footer.contains("1 degraded"), "{footer}");
        assert!(
            !footer.contains("blind"),
            "a lane that worked before is NOT blind: {footer}"
        );
        assert!(
            !footer.contains("jira-sprint"),
            "weather is a count, not a name: {footer}"
        );

        // grafana-alerts has never once succeeded → blind, and gets named.
        store.record_poll(
            crate::suggest::SourceKind::GrafanaAlerts,
            crate::suggest::SourceStatus::AuthMissing,
            2000,
        );
        let footer = bridge.health_footer().expect("blind lanes surface");
        assert!(footer.contains("1 lane blind (never once succeeded)"), "{footer}");
        assert!(footer.contains("grafana-alerts needs auth"), "{footer}");
        assert!(footer.contains("1 degraded"), "both halves coexist: {footer}");
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

    #[test]
    fn ranked_cache_invalidates_when_the_store_changes() {
        let (inproc, _live) = live_inproc();
        let store = Arc::new(SuggestionStore::new());
        store.ingest(
            crate::suggest::SourceKind::GitBranchPr,
            vec![sug(crate::suggest::SourceKind::GitBranchPr, "1", "alpha", "/x")],
            1000,
        );
        let bridge = bridge_with_suggestions(praca::Praca::new(), inproc, Arc::clone(&store), 6);

        let first = bridge.list("", 1000);
        assert_eq!(
            first.iter().filter(|r| matches!(r.kind, RowKind::Suggestion(_))).count(),
            1,
            "first list memoizes one suggestion"
        );

        // Mutate the store (generation bumps) → the memoized snapshot must refresh.
        store.ingest(
            crate::suggest::SourceKind::GitBranchPr,
            vec![
                sug(crate::suggest::SourceKind::GitBranchPr, "1", "alpha", "/x"),
                sug(crate::suggest::SourceKind::GitBranchPr, "2", "beta", "/y"),
            ],
            1100,
        );
        let second = bridge.list("", 1100);
        assert_eq!(
            second.iter().filter(|r| matches!(r.kind, RowKind::Suggestion(_))).count(),
            2,
            "the cache invalidated on the generation change"
        );
    }
}
