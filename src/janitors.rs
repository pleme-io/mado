//! **Session janitors** — vigy-style reactive invariant-holders.
//!
//! A janitor is a typed invariant-holder: it names one runtime invariant
//! ("no ghost sessions linger in the embedded registry", "no suggestion
//! source stays silently broken"), OBSERVES the live state on its own
//! cadence, and publishes every violation as a typed [`JanitorFinding`] on
//! the [`crate::fibers`] bus + `tracing` — **always**, in every mode.
//! REMEDIATION is separately gated by a shadow-first [`Authority`]: the
//! default `Shadow` mode holds every fix (the finding says what WOULD have
//! been done); only an explicit config flip to `Effect` lets a janitor act,
//! and even then only through guarded typed paths that structurally refuse
//! anything a client is attached to.
//!
//! This is the Viggy seven-beat (Observe → Diff → Classify → Decide → Act →
//! Attest → Tick) expressed in-process: the suggest engine thread's
//! maintenance tick is the Tick, the fiber bus + tracing are the Attest
//! surface, and the breathe `Shadow → Effect` promotion posture is the
//! Decide gate.
//!
//! ## Where janitors run (M0 placement decision)
//!
//! Janitors ride the suggest engine thread's EXISTING maintenance tick
//! (`suggest::spawn_engine_thread`'s select loop) with a per-janitor
//! interval gate inside [`JanitorRunner::tick`]. Chosen over registering
//! vigy reconcilers because (a) the tick already exists, runs
//! unconditionally (even with suggestions disabled the loop parks armed),
//! and is hot-reload-aware via `EngineCommand::Swap`; (b) the embedded vigy
//! runtime is config-gated OFF by default (`vigy.enabled = false`), so
//! hanging invariant-holders off it would silently disable them for every
//! default install; (c) janitors need typed access to in-process Rust state
//! (the tear registry, the izumi store) that the tatara-lisp intrinsic
//! surface doesn't expose yet. The **named destination** is `(defvigy …)`
//! authoring on the vigy runtime once those intrinsics exist — this runner
//! is that reconciler's M0.
//!
//! ## The two M0 janitors
//!
//! - [`GhostSessionJanitor`] — watches the embedded tear registry for
//!   fully-exited, zero-subscriber, agent-owned sessions older than a grace
//!   period. tear f9b1f39 (reap-on-exit) + mado 030116a (guarded
//!   `close_session` leaf) killed this class *statically*; the janitor
//!   keeps it dead at RUNTIME (a watched-at-exit-then-detached session, a
//!   race survivor, a pre-fix leftover). Remediation reuses the SAME
//!   guarded close path as the kanshou leaf ([`guarded_close_agent_session`])
//!   — an attached or operator-owned session is refused by construction,
//!   never force-killed.
//! - [`SuggestHealthJanitor`] — watches the izumi store's per-source poll
//!   health and files a row for every lane that has **never once
//!   succeeded** (verdict [`Blind`](crate::suggest::HealthVerdict::Blind))
//!   beyond N consecutive completed polls. A merely-`Degraded` lane files
//!   nothing — weather belongs in the ambient footer, and a row per blip is
//!   what teaches an operator to stop reading the rows. A lane whose
//!   instrument has never run ([`Unknown`](crate::suggest::HealthVerdict::Unknown))
//!   files nothing either, and for a different reason: there is no evidence
//!   to file. Observe-only in M0 (`remediable = false`); the finding surfaces
//!   the broken *declaration* instead of letting it rot silently.
//!
//! ## Tier-honest ledger (a `Result::Err` is mitigation — never round up)
//!
//! - *Truly unrepresentable*: publishing a finding to a nonexistent subject
//!   (the fibers payload IS the address); a remediation write without an
//!   `Authority::Effect` decision has no code path in the runner.
//! - *Only-mitigated*: the attached-session refusal is a runtime guard in
//!   [`guarded_close_agent_session`] (re-checked at close time), not a
//!   type-level impossibility — the honest ceiling while the registry is a
//!   shared mutable map (C4-shaped).
//! - *Observe-only*: `SuggestHealthJanitor` names no remediation at all in
//!   M0 (re-arming a source needs a config/secret fix, an operator move).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use crate::fibers::{BoardEvent, FiberBus, FiberEvent, SessionEvent, Subject};
use crate::suggest::{HealthVerdict, SourceHealth, SourceKind, SourceStatus, Urgency};

// ─────────────────────────────────────────────────────────────────
// Authority — the shadow-first remediation gate
// ─────────────────────────────────────────────────────────────────

/// How much a janitor may DO about a finding. Findings are published in
/// every mode; this gates only the remediation arm (the breathe
/// shadow-first promotion posture, in-process).
#[derive(
    Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Authority {
    /// Observe + publish only — every remediation is held and reported as
    /// [`RemediationOutcome::ShadowHeld`]. The default everywhere.
    #[default]
    Shadow,
    /// Remediations run, through the guarded typed paths only. Flipped per
    /// janitor (or globally) in the `janitors:` config section.
    Effect,
}

impl Authority {
    /// Log label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Authority::Shadow => "shadow",
            Authority::Effect => "effect",
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Janitor catalog (CATALOG REFLECTION — closed, never strings)
// ─────────────────────────────────────────────────────────────────

/// Every janitor the runner knows — the COMPLETE catalog. A new variant is
/// a compile error until [`JanitorKind::slug`] / [`JanitorKind::label`] are
/// extended and a test failure until [`JanitorKind::ALL`] lists it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JanitorKind {
    /// Ghost-session sweeper over the embedded tear registry.
    GhostSession,
    /// Suggestion-source health watcher over the izumi store.
    SuggestHealth,
}

impl JanitorKind {
    /// Every janitor, in catalog (declaration) order — the reflection
    /// surface tests + future tooling iterate (test-only consumer today).
    #[allow(dead_code)]
    pub const ALL: &'static [JanitorKind] = &[JanitorKind::GhostSession, JanitorKind::SuggestHealth];

    /// Kebab slug for logs / config docs.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            JanitorKind::GhostSession => "ghost-session",
            JanitorKind::SuggestHealth => "suggest-health",
        }
    }

    /// Human label (catalog reflection; test/tooling consumer today).
    #[allow(dead_code)]
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            JanitorKind::GhostSession => "ghost-session sweeper",
            JanitorKind::SuggestHealth => "suggestion-source health watcher",
        }
    }
}

/// What KIND of invariant violation a finding names. Distinct from
/// [`JanitorKind`] (one janitor may emit several finding kinds later).
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingKind {
    /// A fully-exited, unwatched, agent-owned session lingering past grace.
    GhostSession,
    /// A suggestion source that has never once observed its upstream,
    /// beyond the consecutive-poll bar. (Kept under its historical name —
    /// the finding CLASS is still "a source is not reporting"; whether it
    /// is blind or merely degraded is the verdict carried in the row's
    /// title/detail/urgency, not a second catalog entry.)
    SourceUnhealthy,
}

// ─────────────────────────────────────────────────────────────────
// Findings + outcomes
// ─────────────────────────────────────────────────────────────────

/// What happened (or deliberately didn't) about one finding. Stamped onto
/// the finding by the runner BEFORE it is published, so every bus event
/// carries its own disposition.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RemediationOutcome {
    /// The finding is not remediable by construction (observe-only janitor).
    ObserveOnly,
    /// Remediable, but authority is [`Authority::Shadow`] — the fix was
    /// HELD. The finding documents what Effect mode would have done.
    ShadowHeld,
    /// The guarded remediation ran and succeeded.
    Applied,
    /// The guarded remediation refused or failed — reason attached. A
    /// refusal (attached / not-agent-owned) is the guard WORKING, not an
    /// error.
    Refused {
        /// Typed-guard label or error string.
        reason: String,
    },
}

impl RemediationOutcome {
    /// Log label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            RemediationOutcome::ObserveOnly => "observe-only",
            RemediationOutcome::ShadowHeld => "shadow-held",
            RemediationOutcome::Applied => "applied",
            RemediationOutcome::Refused { .. } => "refused",
        }
    }
}

/// One observed invariant violation — the [`Subject::Janitors`] payload.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JanitorFinding {
    /// Which janitor observed it.
    pub janitor: JanitorKind,
    /// Which invariant class it violates.
    pub kind: FindingKind,
    /// Stable dedup key (`janitor:<slug>:<target>`) — the board-row
    /// identity across re-observations.
    pub key: String,
    /// One-line description (board row title).
    pub title: String,
    /// Secondary context (board row detail).
    pub detail: String,
    /// How urgently the finding wants operator attention.
    pub urgency: Urgency,
    /// Whether a guarded remediation exists for this finding.
    pub remediable: bool,
    /// The typed target the remediation would act on (session id / source
    /// slug). `None` only for findings with no actionable object.
    pub target: Option<String>,
    /// When the janitor observed it (unix ms).
    pub observed_ms: u64,
    /// Disposition — stamped by the runner before publish.
    pub outcome: RemediationOutcome,
}

impl JanitorFinding {
    /// A non-remediable observation (outcome starts [`RemediationOutcome::ObserveOnly`]).
    #[must_use]
    pub fn observation(
        janitor: JanitorKind,
        kind: FindingKind,
        key: String,
        title: String,
        detail: String,
        urgency: Urgency,
        observed_ms: u64,
    ) -> Self {
        Self {
            janitor,
            kind,
            key,
            title,
            detail,
            urgency,
            remediable: false,
            target: None,
            observed_ms,
            outcome: RemediationOutcome::ObserveOnly,
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// The guarded close path (shared with the kanshou close_session leaf)
// ─────────────────────────────────────────────────────────────────

/// Outcome of a guarded agent-session close — the ONE close path the
/// kanshou `close_session` leaf and the ghost janitor both flow through
/// (solve-once: the typed guards live here, never duplicated).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GuardedClose {
    /// Session removed from the registry.
    Closed,
    /// No session with that id (already gone — the invariant holds).
    NoSuchSession,
    /// The session is not [`tear_types::SessionSource::Agent`]-owned;
    /// operator sessions are structurally out of reach of this path.
    NotAgentOwned {
        /// The refusing session's source label.
        source: String,
    },
    /// A client is attached to the session's byte stream (a mado window, a
    /// recorder) — never yanked.
    Attached {
        /// Live subscriber count observed at refusal time.
        subscribers: u32,
    },
    /// The backend `kill_session` itself failed.
    Error(String),
}

/// Close an agent-owned session in the embedded tear registry, guarded:
/// only `SessionSource::Agent` sessions close, and only with ZERO live pane
/// subscribers. Both guards re-check live state at call time (the runtime
/// mirror of the kanshou leaf's contract — see mado 030116a).
pub fn guarded_close_agent_session(
    inproc: &tear_core::InProcess,
    sid: tear_types::SessionId,
) -> GuardedClose {
    use tear_types::MultiplexerControl;
    let looked_up = inproc.with_registry(|r| {
        r.sessions
            .get(&sid)
            .map(|s| (s.source.clone(), s.panes.keys().copied().collect::<Vec<_>>()))
    });
    let Some((source, panes)) = looked_up else {
        return GuardedClose::NoSuchSession;
    };
    if !matches!(source, tear_types::SessionSource::Agent) {
        return GuardedClose::NotAgentOwned {
            source: source.label().to_owned(),
        };
    }
    let subscribers: u32 = panes
        .iter()
        .map(|p| inproc.pane_subscriber_count(*p).unwrap_or(0))
        .sum();
    if subscribers > 0 {
        return GuardedClose::Attached { subscribers };
    }
    match inproc.kill_session(sid) {
        Ok(()) => GuardedClose::Closed,
        Err(e) => GuardedClose::Error(e.to_string()),
    }
}

// ─────────────────────────────────────────────────────────────────
// The mockable environment border (TYPED-SPEC Environment idiom)
// ─────────────────────────────────────────────────────────────────

/// A janitor's-eye view of one live session (projected out of the tear
/// registry so tests never need a real PTY).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionView {
    /// Session id, display form.
    pub id: String,
    /// Operator-visible name.
    pub name: String,
    /// `SessionSource::Agent`?
    pub agent_owned: bool,
    /// Every pane `Exited` (and at least one pane exists)?
    pub fully_exited: bool,
    /// Sum of live pane byte-stream subscribers.
    pub subscribers: u32,
}

/// The janitors' I/O boundary — every side effect a janitor can observe or
/// perform, behind one trait so tests drive both janitors against canned
/// state and recorded writes (the TYPED-SPEC + INTERPRETER triplet's
/// `Environment` seam).
pub trait JanitorEnv {
    /// Snapshot of the embedded tear registry (empty when this process has
    /// no embedded registry — `mado mcp`, daemon mode).
    fn tear_sessions(&self) -> Vec<SessionView>;
    /// Run the guarded close against a session id (display form).
    fn close_agent_session(&self, id: &str) -> GuardedClose;
    /// The izumi store's per-source poll health.
    fn suggest_health(&self) -> Vec<(SourceKind, SourceHealth)>;
    /// Project a finding onto the Ctrl-S board (the agent-lane
    /// `suggest::inject` path).
    fn project_to_board(&self, finding: &JanitorFinding);
}

/// The live GUI's embedded tear registry, published once by
/// `gui_tear_attach` right where the kanshou aggregator gets its copy.
/// Never set in `mado mcp` / daemon mode — janitors then observe an empty
/// session world (graceful, structural).
static TEAR_INPROC: OnceLock<Arc<tear_core::InProcess>> = OnceLock::new();

/// Publish the live embedded registry to the janitor plane (idempotent,
/// first set wins — mirrors `MadoAppState::set_tear_inproc`).
pub fn set_tear_inproc(inproc: Arc<tear_core::InProcess>) {
    let _ = TEAR_INPROC.set(inproc);
}

/// The real environment: reads the process-global tear registry + izumi
/// store, writes through the guarded close + the agent-lane inject.
pub struct RealJanitorEnv;

impl JanitorEnv for RealJanitorEnv {
    fn tear_sessions(&self) -> Vec<SessionView> {
        use tear_types::MultiplexerControl;
        let Some(inproc) = TEAR_INPROC.get() else {
            return Vec::new();
        };
        // Project under the registry lock, count subscribers OUTSIDE it
        // (pane_subscriber_count takes its own lock — never nested).
        let rows: Vec<(String, String, bool, bool, Vec<tear_types::PaneId>)> = inproc
            .with_registry(|r| {
                r.sessions
                    .values()
                    .map(|s| {
                        (
                            s.id.to_string(),
                            s.name.clone(),
                            matches!(s.source, tear_types::SessionSource::Agent),
                            !s.panes.is_empty()
                                && s.panes
                                    .values()
                                    .all(|p| matches!(p.state, tear_types::PaneState::Exited { .. })),
                            s.panes.keys().copied().collect(),
                        )
                    })
                    .collect()
            });
        rows.into_iter()
            .map(|(id, name, agent_owned, fully_exited, panes)| SessionView {
                id,
                name,
                agent_owned,
                fully_exited,
                subscribers: panes
                    .iter()
                    .map(|p| inproc.pane_subscriber_count(*p).unwrap_or(0))
                    .sum(),
            })
            .collect()
    }

    fn close_agent_session(&self, id: &str) -> GuardedClose {
        let Some(inproc) = TEAR_INPROC.get() else {
            return GuardedClose::NoSuchSession;
        };
        let Ok(sid) = id.parse::<tear_types::SessionId>() else {
            return GuardedClose::Error(String::from("invalid session id"));
        };
        guarded_close_agent_session(inproc, sid)
    }

    fn suggest_health(&self) -> Vec<(SourceKind, SourceHealth)> {
        crate::suggest::store().health()
    }

    fn project_to_board(&self, finding: &JanitorFinding) {
        // The existing agent-lane ingress: stable key ⇒ re-projection is an
        // idempotent upsert, so a persistent finding is ONE living row, not
        // a pile.
        let urgency = match serde_json::to_value(finding.urgency) {
            Ok(serde_json::Value::String(s)) => Some(s),
            Ok(_) | Err(_) => None,
        };
        let mut session_name = String::from("\u{1F9F9} "); // 🧹 janitor lane marker
        session_name.push_str(finding.title.chars().take(40).collect::<String>().trim());
        let params = crate::suggest::InjectParams {
            title: finding.title.clone(),
            key: Some(finding.key.clone()),
            detail: Some(finding.detail.clone()),
            urgency,
            cwd: None,
            session_name: Some(session_name),
            command: None,
        };
        if let Err(e) = crate::suggest::inject(params) {
            tracing::warn!(key = %finding.key, err = %e, "janitor board projection failed");
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// The Janitor trait
// ─────────────────────────────────────────────────────────────────

/// A typed invariant-holder. `observe` is pure over the [`JanitorEnv`]
/// (findings out, no writes); `remediate` is the ONLY writing entry point
/// and is invoked solely by the runner, solely under
/// [`Authority::Effect`] — a remediation without that decision has no code
/// path.
pub trait Janitor: Send {
    /// Which catalog entry this is.
    fn kind(&self) -> JanitorKind;
    /// The fiber subjects this janitor's activity is published on (the
    /// subject filter — findings always on [`Subject::Janitors`], plus any
    /// effect subjects).
    fn subjects(&self) -> &'static [Subject];
    /// Observe the invariant; return every current violation. MUST NOT
    /// write through the env.
    fn observe(&mut self, env: &dyn JanitorEnv, now_ms: u64) -> Vec<JanitorFinding>;
    /// Attempt the guarded fix for one finding (Effect mode only — the
    /// runner is the sole caller and the sole gate).
    fn remediate(&mut self, env: &dyn JanitorEnv, finding: &JanitorFinding) -> RemediationOutcome;
}

// ─────────────────────────────────────────────────────────────────
// GhostSessionJanitor
// ─────────────────────────────────────────────────────────────────

/// Watches the embedded tear registry for agent-owned sessions whose every
/// pane has exited, that nothing is watching, and that have stayed that way
/// past a grace period. Grace is anchored at the FIRST tick the session was
/// seen ghost (janitor-side clock — the registry keeps no exit timestamp),
/// so a session must hold the ghost predicate across the whole window; one
/// that revives (new pane, new subscriber) resets.
pub struct GhostSessionJanitor {
    grace_ms: u64,
    /// session id → first tick (ms) it was observed ghost.
    first_ghost_ms: BTreeMap<String, u64>,
}

impl GhostSessionJanitor {
    /// A sweeper holding `grace_secs` of tolerance before a ghost is
    /// reported (and, in Effect mode, closed).
    #[must_use]
    pub fn new(grace_secs: u64) -> Self {
        Self {
            grace_ms: grace_secs.saturating_mul(1000),
            first_ghost_ms: BTreeMap::new(),
        }
    }

    fn is_ghost(v: &SessionView) -> bool {
        v.agent_owned && v.fully_exited && v.subscribers == 0
    }
}

impl Janitor for GhostSessionJanitor {
    fn kind(&self) -> JanitorKind {
        JanitorKind::GhostSession
    }

    fn subjects(&self) -> &'static [Subject] {
        &[Subject::Janitors, Subject::Sessions]
    }

    fn observe(&mut self, env: &dyn JanitorEnv, now_ms: u64) -> Vec<JanitorFinding> {
        let views = env.tear_sessions();
        let ghosts: BTreeMap<String, &SessionView> = views
            .iter()
            .filter(|v| Self::is_ghost(v))
            .map(|v| (v.id.clone(), v))
            .collect();
        // A session that stopped being ghost (revived / already reaped)
        // resets its grace clock by leaving the tracker.
        self.first_ghost_ms.retain(|id, _| ghosts.contains_key(id));
        let mut out = Vec::new();
        for (id, v) in &ghosts {
            let born = *self.first_ghost_ms.entry(id.clone()).or_insert(now_ms);
            if now_ms.saturating_sub(born) < self.grace_ms {
                continue;
            }
            let mut key = String::from("janitor:ghost-session:");
            key.push_str(id);
            let mut title = String::from("ghost session: ");
            title.push_str(&v.name);
            let mut detail = String::from("agent session ");
            detail.push_str(id);
            detail.push_str(" fully exited, unwatched for ");
            detail.push_str(&(now_ms.saturating_sub(born) / 1000).to_string());
            detail.push_str("s (grace ");
            detail.push_str(&(self.grace_ms / 1000).to_string());
            detail.push_str("s)");
            out.push(JanitorFinding {
                janitor: JanitorKind::GhostSession,
                kind: FindingKind::GhostSession,
                key,
                title,
                detail,
                urgency: Urgency::Low,
                remediable: true,
                target: Some(id.clone()),
                observed_ms: now_ms,
                outcome: RemediationOutcome::ObserveOnly,
            });
        }
        out
    }

    fn remediate(&mut self, env: &dyn JanitorEnv, finding: &JanitorFinding) -> RemediationOutcome {
        let Some(target) = finding.target.as_deref() else {
            return RemediationOutcome::Refused {
                reason: String::from("finding carries no target session id"),
            };
        };
        match env.close_agent_session(target) {
            GuardedClose::Closed => {
                self.first_ghost_ms.remove(target);
                RemediationOutcome::Applied
            }
            GuardedClose::NoSuchSession => {
                // Already gone (raced tear's own reap) — the invariant
                // holds, but WE didn't apply anything: say so honestly.
                self.first_ghost_ms.remove(target);
                RemediationOutcome::Refused {
                    reason: String::from("no-such-session (already reaped)"),
                }
            }
            GuardedClose::NotAgentOwned { source } => {
                let mut reason = String::from("not-agent-owned: ");
                reason.push_str(&source);
                RemediationOutcome::Refused { reason }
            }
            GuardedClose::Attached { subscribers } => {
                // The load-bearing guard: never yank a watched session.
                let mut reason = String::from("attached (");
                reason.push_str(&subscribers.to_string());
                reason.push_str(" subscribers)");
                RemediationOutcome::Refused { reason }
            }
            GuardedClose::Error(e) => RemediationOutcome::Refused { reason: e },
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// SuggestHealthJanitor
// ─────────────────────────────────────────────────────────────────

/// Per-source poll tracking for [`SuggestHealthJanitor`].
#[derive(Default, Clone, Copy)]
struct PollTrack {
    /// `last_poll_ms` at our previous observation — a change means at
    /// least one NEW poll completed since we last looked. `None` = this
    /// janitor has not observed this source at all yet.
    ///
    /// `Option`, not a bare `0` sentinel. `0` is itself a meaningful
    /// `last_poll_ms` — it is what
    /// [`HealthVerdict::Unknown`](crate::suggest::HealthVerdict::Unknown)
    /// rests on, "the instrument never ran" — so a `0` default made
    /// *never-observed-by-this-janitor* and *never-polled-at-all* the same
    /// value, and the streak bar therefore doubled, silently, as the
    /// never-polled guard. That is the same collapse-two-cases-into-one-slot
    /// bug this whole health plane exists to undo, one level down, and it
    /// had a concrete cost: with the sentinel, deleting the `Unknown` arm in
    /// [`SuggestHealthJanitor::observe`] changed no observable behaviour, so
    /// the carve-out was untestable and free to rot. Typed, the verdict is
    /// the only thing keeping a no-evidence lane off the board.
    last_poll_ms: Option<u64>,
    /// Consecutive completed polls observed unhealthy.
    consecutive_bad: u32,
}

/// Watches the izumi store's per-source health and files a board row for
/// every lane that has **never once succeeded**.
///
/// Two independent bars, both of which must be cleared:
///
/// 1. **N consecutive completed polls** in `Error` / `AuthMissing` /
///    `TimedOut`. Counts POLLS, not janitor ticks: an observation only
///    advances the counter when the source's `last_poll_ms` moved, so a
///    slow source isn't condemned by a fast janitor. **This bar is no longer
///    load-bearing.** It used to be the mitigation for a per-process success
///    latch — izumi persisted `entries` and dropped `health`, so a restart
///    made every degraded lane read `Blind` until its next good poll, and
///    the streak was what bounded the mis-read. izumi `c2b48c0` persists the
///    health plane, so the latch is a fact about the SOURCE and this bar
///    reverts to what its name always said: debouncing a flapping upstream.
/// 2. **The verdict is [`Blind`](crate::suggest::HealthVerdict::Blind)** —
///    the source has been polled and has no successful poll on record. A
///    `Degraded` lane (it worked before, it is failing now) is weather and
///    files NOTHING; it surfaces only in the picker's ambient health footer.
///    An [`Unknown`](crate::suggest::HealthVerdict::Unknown) lane — no poll
///    has ever completed — files nothing either: it is the absence of a
///    finding, not a quiet one, and a gate that fires on it fires on nothing.
///
/// `Unconfigured` is deliberately NOT in bar 1's status set, so an
/// armed-but-unparameterized lane never files a row. That is a *chosen*
/// state, not a surprising one: `SuggestionsConfig::prescribed()` arms 26
/// sources on purpose and lets each degrade to "needs config", so the
/// operator who never set a `base_url` already knows. The row is reserved
/// for the surprising case — a lane you believed was working. Unconfigured
/// lanes still appear in the footer.
///
/// Observe-only in M0 — re-arming a source is an operator move (fix the
/// token / config), so the janitor surfaces it rather than pretending to
/// fix it.
pub struct SuggestHealthJanitor {
    min_consecutive: u32,
    seen: BTreeMap<SourceKind, PollTrack>,
}

impl SuggestHealthJanitor {
    /// A watcher reporting a source after `min_consecutive_polls` bad polls
    /// in a row (floored at 1) — a flap debounce, not a correctness bar; see
    /// the type's doc for why that changed.
    #[must_use]
    pub fn new(min_consecutive_polls: u32) -> Self {
        Self {
            min_consecutive: min_consecutive_polls.max(1),
            seen: BTreeMap::new(),
        }
    }
}

impl Janitor for SuggestHealthJanitor {
    fn kind(&self) -> JanitorKind {
        JanitorKind::SuggestHealth
    }

    fn subjects(&self) -> &'static [Subject] {
        &[Subject::Janitors]
    }

    fn observe(&mut self, env: &dyn JanitorEnv, now_ms: u64) -> Vec<JanitorFinding> {
        let mut out = Vec::new();
        for (kind, health) in env.suggest_health() {
            let track = self.seen.entry(kind).or_default();
            if track.last_poll_ms != Some(health.last_poll_ms) {
                track.last_poll_ms = Some(health.last_poll_ms);
                let bad = matches!(
                    health.status,
                    SourceStatus::Error | SourceStatus::AuthMissing | SourceStatus::TimedOut
                );
                track.consecutive_bad = if bad {
                    track.consecutive_bad.saturating_add(1)
                } else {
                    0
                };
            }
            // ── THE GATE: only a BLIND lane earns a row ──────────────
            //
            // One EXHAUSTIVE match, so a fifth verdict is a compile error
            // here rather than a silent default. The verdict itself comes
            // from ONE definition (`izumi::HealthVerdict::of`, reached via
            // `SourceHealth::verdict()`) shared with the picker footer and
            // `board_json`, so the operator face and the agent face cannot
            // drift apart about which lanes are dead.
            match health.verdict() {
                // Polled, and never once observed to succeed. A finding
                // about the SOURCE — a wrong context, a dead URL, a
                // credential that was never right. This is the row.
                HealthVerdict::Blind => {}
                // No poll has ever completed. THIS ARM IS THE CARVE-OUT,
                // and it must never be merged into `Blind`.
                //
                // It is the same skip this loop always had — it used to be
                // a bare `if health.last_poll_ms == 0 { continue }` at the
                // top, an incidental guard with a comment ("never polled
                // this process lifetime") and no type behind it. izumi
                // `c2b48c0` gave the case a name: `OkEvidence::Unobserved`
                // → `HealthVerdict::Unknown`, evidence in NEITHER
                // direction. Condemning on it is condemning on nothing,
                // which is exactly the gate-on-no-evidence bug that commit
                // fixed upstream; reintroducing the fold here would import
                // it straight back. Blind is a finding, unknown is the
                // absence of one, and only a finding may gate.
                //
                // Measured, not asserted: folding this arm into `Blind`
                // above turns
                // `a_never_polled_lane_is_unknown_and_files_nothing_while_a_blind_one_files`
                // red with
                // `left: ["janitor:suggest-health:grafana-alerts",
                // "janitor:suggest-health:jira-sprint"]` against
                // `right: ["janitor:suggest-health:grafana-alerts"]` — the
                // never-polled lane files a High-urgency row claiming it
                // "has never once succeeded".
                HealthVerdict::Unknown => continue,
                // Reporting normally, or weather: a Degraded lane HAS
                // observed its upstream and is failing now. It gets the
                // ambient footer and nothing else. Filing a row per blip is
                // precisely how a health mechanism trains its reader to
                // ignore it — which is how this class stayed invisible in
                // the first place: the row that fired said "erroring", the
                // same word a healthy lane says during a thirty-second
                // blip, so the rows that mattered were indistinguishable
                // from the ones that didn't.
                HealthVerdict::Ok | HealthVerdict::Degraded => continue,
            }
            if track.consecutive_bad < self.min_consecutive {
                continue;
            }
            let mut key = String::from("janitor:suggest-health:");
            key.push_str(kind.slug());
            // Say it in WORDS, on the row. The operator must not have to
            // decode a status label to learn that this lane has never
            // worked — that inference is the thing nobody made.
            let mut title = String::from("blind source: ");
            title.push_str(kind.slug());
            title.push_str(" has never once succeeded");
            let mut detail = String::from("status \u{201C}");
            detail.push_str(health.status.label());
            detail.push_str("\u{201D} for ");
            detail.push_str(&track.consecutive_bad.to_string());
            detail.push_str(if track.consecutive_bad == 1 {
                " poll"
            } else {
                " consecutive polls"
            });
            detail.push_str(
                " and not one success \u{2014} a declaration to fix, \
                 not an outage to wait out",
            );
            // Blind ALWAYS outranks the old status-derived scale. The
            // previous mapping was inverted for the worst case: it sent
            // AuthMissing (the likeliest never-once-worked cause — a
            // credential that was never right) to Urgency::Low, BELOW a
            // transient Error. Needing intervention is the whole signal,
            // so it sets the urgency by itself.
            out.push(JanitorFinding::observation(
                JanitorKind::SuggestHealth,
                FindingKind::SourceUnhealthy,
                key,
                title,
                detail,
                Urgency::High,
                now_ms,
            ));
        }
        out
    }

    fn remediate(&mut self, _env: &dyn JanitorEnv, _finding: &JanitorFinding) -> RemediationOutcome {
        // Named honestly: no remediation exists in M0 (the fix is a
        // config/secret change only the operator can make).
        RemediationOutcome::ObserveOnly
    }
}

// ─────────────────────────────────────────────────────────────────
// The runner — per-janitor cadence + the shadow gate
// ─────────────────────────────────────────────────────────────────

/// One scheduled janitor: the holder plus its cadence + authority knobs
/// (resolved from config at build time; a config hot-swap rebuilds the
/// whole runner).
struct Slot {
    janitor: Box<dyn Janitor>,
    interval_ms: u64,
    authority: Authority,
    last_run_ms: u64,
}

/// Drives every enabled janitor on the host tick (the suggest engine
/// thread's maintenance tick), gating each on its own interval. Findings
/// ALWAYS publish (bus + tracing); remediation runs only under
/// [`Authority::Effect`]; board projection is config-gated.
pub struct JanitorRunner {
    slots: Vec<Slot>,
    board_rows: bool,
}

impl JanitorRunner {
    /// Build the runner from the typed `janitors:` config section. A
    /// disabled plane (or bare tier) yields an empty runner — ticks are
    /// then free no-ops.
    #[must_use]
    pub fn from_config(cfg: &crate::config::JanitorsConfig) -> Self {
        let mut slots: Vec<Slot> = Vec::new();
        if cfg.enabled {
            if cfg.ghost_session.enabled {
                slots.push(Slot {
                    janitor: Box::new(GhostSessionJanitor::new(cfg.ghost_session.grace_secs)),
                    interval_ms: cfg.ghost_session.interval_secs.max(1).saturating_mul(1000),
                    authority: cfg.ghost_session.authority.unwrap_or(cfg.authority),
                    last_run_ms: 0,
                });
            }
            if cfg.suggest_health.enabled {
                slots.push(Slot {
                    janitor: Box::new(SuggestHealthJanitor::new(
                        cfg.suggest_health.min_consecutive_polls,
                    )),
                    interval_ms: cfg.suggest_health.interval_secs.max(1).saturating_mul(1000),
                    authority: cfg.suggest_health.authority.unwrap_or(cfg.authority),
                    last_run_ms: 0,
                });
            }
        }
        for s in &slots {
            let subjects: Vec<&'static str> =
                s.janitor.subjects().iter().map(|x| x.slug()).collect();
            tracing::info!(
                janitor = s.janitor.kind().slug(),
                authority = s.authority.label(),
                interval_ms = s.interval_ms,
                subjects = ?subjects,
                "janitor armed"
            );
        }
        Self {
            slots,
            board_rows: cfg.board_rows,
        }
    }

    /// Whether any janitor is armed (observability / tests).
    #[allow(dead_code)] // test-consumed today; an MCP/status leaf is named follow-up
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.slots.is_empty()
    }

    /// One host tick: run every janitor whose interval has elapsed, publish
    /// every finding (bus + tracing), remediate under Effect, project onto
    /// the board when configured. Returns the number of findings processed.
    pub fn tick(&mut self, env: &dyn JanitorEnv, bus: &FiberBus, now_ms: u64) -> usize {
        let mut processed = 0_usize;
        let board_rows = self.board_rows;
        for slot in &mut self.slots {
            if slot.last_run_ms != 0
                && now_ms.saturating_sub(slot.last_run_ms) < slot.interval_ms
            {
                continue;
            }
            slot.last_run_ms = now_ms;
            for mut finding in slot.janitor.observe(env, now_ms) {
                // Decide (the shadow-first gate): a write without an
                // explicit Effect authority has no path through here.
                let outcome = if !finding.remediable {
                    RemediationOutcome::ObserveOnly
                } else if slot.authority == Authority::Effect {
                    slot.janitor.remediate(env, &finding)
                } else {
                    RemediationOutcome::ShadowHeld
                };
                tracing::info!(
                    janitor = finding.janitor.slug(),
                    key = %finding.key,
                    authority = slot.authority.label(),
                    outcome = outcome.label(),
                    title = %finding.title,
                    "janitor finding"
                );
                // Effect facts get their own session-lifecycle event.
                if outcome == RemediationOutcome::Applied
                    && finding.kind == FindingKind::GhostSession
                {
                    if let Some(target) = finding.target.clone() {
                        bus.publish(FiberEvent::Session(SessionEvent::GhostSessionReaped {
                            session_id: target,
                        }));
                    }
                }
                // Board projection (config-gated): the finding surfaces on
                // Ctrl-S as a stable agent-lane row.
                if board_rows {
                    env.project_to_board(&finding);
                    bus.publish(FiberEvent::Board(BoardEvent::JanitorRowInjected {
                        key: finding.key.clone(),
                        title: finding.title.clone(),
                    }));
                }
                // Findings ALWAYS publish, disposition attached.
                finding.outcome = outcome;
                bus.publish(FiberEvent::Janitor(finding));
                processed += 1;
            }
        }
        processed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::sync::broadcast::error::TryRecvError;

    use super::*;
    use crate::config::{
        GhostSessionJanitorConfig, JanitorsConfig, SuggestHealthJanitorConfig,
    };

    // ── Mock environment ───────────────────────────────────────────

    #[derive(Default)]
    struct MockJanitorEnv {
        sessions: Mutex<Vec<SessionView>>,
        health: Mutex<Vec<(SourceKind, SourceHealth)>>,
        /// Close results by session id; a close against an id not present
        /// answers `NoSuchSession`.
        close_results: Mutex<BTreeMap<String, GuardedClose>>,
        closes: Mutex<Vec<String>>,
        projected: Mutex<Vec<String>>,
    }

    impl MockJanitorEnv {
        fn with_sessions(sessions: Vec<SessionView>) -> Self {
            let env = Self::default();
            *env.sessions.lock().unwrap() = sessions;
            env
        }

        fn closes(&self) -> Vec<String> {
            self.closes.lock().unwrap().clone()
        }

        fn projected(&self) -> Vec<String> {
            self.projected.lock().unwrap().clone()
        }
    }

    impl JanitorEnv for MockJanitorEnv {
        fn tear_sessions(&self) -> Vec<SessionView> {
            self.sessions.lock().unwrap().clone()
        }

        fn close_agent_session(&self, id: &str) -> GuardedClose {
            self.closes.lock().unwrap().push(id.to_owned());
            let result = self
                .close_results
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .unwrap_or(GuardedClose::Closed);
            if result == GuardedClose::Closed {
                self.sessions.lock().unwrap().retain(|s| s.id != id);
            }
            result
        }

        fn suggest_health(&self) -> Vec<(SourceKind, SourceHealth)> {
            self.health.lock().unwrap().clone()
        }

        fn project_to_board(&self, finding: &JanitorFinding) {
            self.projected.lock().unwrap().push(finding.key.clone());
        }
    }

    fn ghost_view(id: &str) -> SessionView {
        SessionView {
            id: id.to_owned(),
            name: String::from("sh"),
            agent_owned: true,
            fully_exited: true,
            subscribers: 0,
        }
    }

    /// A lane with NO successful poll on record. `last_poll_ms > 0` ⇒ the
    /// `Blind` verdict (polled, never succeeded); `last_poll_ms == 0` ⇒
    /// `Unknown` (the instrument never ran) — the two the janitor must keep
    /// apart. `first_poll_ms == last_poll_ms` reproduces a source seen for
    /// the first time at that stamp, which is the weakest window the claim
    /// can rest on.
    fn health_row(status: SourceStatus, last_poll_ms: u64) -> SourceHealth {
        SourceHealth {
            status,
            last_poll_ms,
            last_ok_ms: 0,
            first_poll_ms: last_poll_ms,
        }
    }

    /// A lane that HAS observed its upstream before and is failing now —
    /// the `Degraded` verdict. Note the original helper could not express
    /// this at all (it hardcodes `last_ok_ms: 0`), which is a small piece
    /// of why the two cases went unseparated for so long.
    fn degraded_row(status: SourceStatus, last_poll_ms: u64, last_ok_ms: u64) -> SourceHealth {
        SourceHealth {
            status,
            last_poll_ms,
            last_ok_ms,
            first_poll_ms: last_ok_ms.min(last_poll_ms),
        }
    }

    // ── Catalog reflection ─────────────────────────────────────────

    #[test]
    fn janitor_catalog_is_complete_with_unique_slugs() {
        assert_eq!(JanitorKind::ALL.len(), 2);
        let mut slugs: Vec<&str> = JanitorKind::ALL.iter().map(|k| k.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), JanitorKind::ALL.len(), "slug collision");
        // Serde wire form = kebab slug (config/tooling round-trip).
        assert_eq!(
            serde_json::to_string(&JanitorKind::GhostSession).unwrap(),
            "\"ghost-session\""
        );
        // Every catalog janitor declares its fiber subjects.
        assert!(GhostSessionJanitor::new(0).subjects().contains(&Subject::Sessions));
        assert!(SuggestHealthJanitor::new(1).subjects().contains(&Subject::Janitors));
    }

    // ── GhostSessionJanitor ────────────────────────────────────────

    #[test]
    fn ghost_inside_grace_period_is_not_reported() {
        let env = MockJanitorEnv::with_sessions(vec![ghost_view("aaaa")]);
        let mut j = GhostSessionJanitor::new(120);
        // First sighting anchors the clock — no finding yet.
        assert!(j.observe(&env, 1_000_000).is_empty());
        // Still inside grace.
        assert!(j.observe(&env, 1_000_000 + 119_000).is_empty());
    }

    #[test]
    fn ghost_past_grace_yields_a_remediable_finding() {
        let env = MockJanitorEnv::with_sessions(vec![ghost_view("aaaa")]);
        let mut j = GhostSessionJanitor::new(120);
        assert!(j.observe(&env, 1_000_000).is_empty());
        let findings = j.observe(&env, 1_000_000 + 120_000);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.kind, FindingKind::GhostSession);
        assert_eq!(f.key, "janitor:ghost-session:aaaa");
        assert_eq!(f.target.as_deref(), Some("aaaa"));
        assert!(f.remediable);
    }

    #[test]
    fn revived_session_resets_the_grace_clock() {
        let env = MockJanitorEnv::with_sessions(vec![ghost_view("aaaa")]);
        let mut j = GhostSessionJanitor::new(120);
        assert!(j.observe(&env, 0).is_empty());
        // A subscriber attaches (mado window opened onto it) — not ghost.
        env.sessions.lock().unwrap()[0].subscribers = 1;
        assert!(j.observe(&env, 60_000).is_empty());
        // Detaches again: the clock must restart, not carry the old anchor.
        env.sessions.lock().unwrap()[0].subscribers = 0;
        assert!(j.observe(&env, 130_000).is_empty(), "clock must have reset");
        assert_eq!(j.observe(&env, 130_000 + 120_000).len(), 1);
    }

    #[test]
    fn attached_or_operator_sessions_are_never_ghost_candidates() {
        let attached = SessionView {
            subscribers: 2,
            ..ghost_view("attached")
        };
        let human = SessionView {
            agent_owned: false,
            ..ghost_view("human")
        };
        let running = SessionView {
            fully_exited: false,
            ..ghost_view("running")
        };
        let env = MockJanitorEnv::with_sessions(vec![attached, human, running]);
        let mut j = GhostSessionJanitor::new(0);
        assert!(j.observe(&env, 0).is_empty());
        assert!(j.observe(&env, 999_000).is_empty());
        assert!(env.closes().is_empty());
    }

    #[test]
    fn remediate_refuses_when_the_guard_reports_attached() {
        let env = MockJanitorEnv::with_sessions(vec![ghost_view("aaaa")]);
        env.close_results
            .lock()
            .unwrap()
            .insert(String::from("aaaa"), GuardedClose::Attached { subscribers: 1 });
        let mut j = GhostSessionJanitor::new(0);
        j.observe(&env, 0);
        let findings = j.observe(&env, 1_000);
        let out = j.remediate(&env, &findings[0]);
        match out {
            RemediationOutcome::Refused { reason } => assert!(reason.contains("attached")),
            other => panic!("expected Refused, got {other:?}"),
        }
        // The session must still exist (never force-killed).
        assert_eq!(env.tear_sessions().len(), 1);
    }

    // ── SuggestHealthJanitor ───────────────────────────────────────

    #[test]
    fn healthy_and_never_polled_sources_are_silent() {
        let env = MockJanitorEnv::default();
        *env.health.lock().unwrap() = vec![
            (SourceKind::TendRepos, health_row(SourceStatus::Ok, 10_000)),
            (SourceKind::JiraSprint, health_row(SourceStatus::Error, 0)), // never polled
        ];
        let mut j = SuggestHealthJanitor::new(3);
        assert!(j.observe(&env, 20_000).is_empty());
        assert!(j.observe(&env, 30_000).is_empty());
    }

    /// **The carve-out, and the one that must not be folded away.** Two lanes
    /// with the SAME status and the SAME (zero) success history, separated by
    /// one fact: one has been polled and one never has.
    ///
    /// * polled + never once succeeded → `Blind` → a finding → **files a row**;
    /// * never polled → `Unknown` → no evidence → **files nothing**.
    ///
    /// The debounce is set to 1 on purpose so it cannot be what keeps the
    /// second lane quiet: both lanes clear the streak bar on their first
    /// observation (see [`PollTrack::last_poll_ms`] — that is precisely why it
    /// is an `Option`). The ONLY thing standing between a never-polled lane
    /// and a High-urgency board row saying it "has never once succeeded" is
    /// the `HealthVerdict::Unknown` arm in `observe`. Fold that arm into
    /// `Blind` and this test goes red on the extra finding — which is the
    /// gate-on-no-evidence bug izumi `c2b48c0` fixed upstream, re-imported.
    #[test]
    fn a_never_polled_lane_is_unknown_and_files_nothing_while_a_blind_one_files() {
        let env = MockJanitorEnv::default();
        let polled = health_row(SourceStatus::Error, 10_000);
        let never_polled = health_row(SourceStatus::Error, 0);
        // The verdicts the rows carry — izumi's rule, asserted here so a
        // failure names WHICH half broke.
        assert_eq!(polled.verdict(), crate::suggest::HealthVerdict::Blind);
        assert_eq!(
            never_polled.verdict(),
            crate::suggest::HealthVerdict::Unknown,
            "a lane whose instrument never ran is unknown, never blind"
        );
        assert!(!never_polled.ever_polled());
        *env.health.lock().unwrap() = vec![
            (SourceKind::GrafanaAlerts, polled),
            (SourceKind::JiraSprint, never_polled),
        ];
        // Debounce of 1: the streak bar is cleared by BOTH lanes.
        let mut j = SuggestHealthJanitor::new(1);
        let findings = j.observe(&env, 10_500);
        let keys: Vec<&str> = findings.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["janitor:suggest-health:grafana-alerts"],
            "exactly the blind lane files; the never-polled lane must not"
        );
        assert!(
            !keys.contains(&"janitor:suggest-health:jira-sprint"),
            "a row condemning a lane nobody has looked at is a row about nothing"
        );
    }

    #[test]
    fn unhealthy_source_reports_only_after_n_consecutive_polls() {
        let env = MockJanitorEnv::default();
        let mut j = SuggestHealthJanitor::new(3);
        for (i, poll_ms) in [10_000_u64, 20_000, 30_000].iter().enumerate() {
            *env.health.lock().unwrap() =
                vec![(SourceKind::GrafanaAlerts, health_row(SourceStatus::Error, *poll_ms))];
            let findings = j.observe(&env, poll_ms + 500);
            if i < 2 {
                assert!(findings.is_empty(), "poll {i} must stay silent");
            } else {
                assert_eq!(findings.len(), 1);
                let f = &findings[0];
                assert_eq!(f.kind, FindingKind::SourceUnhealthy);
                assert_eq!(f.key, "janitor:suggest-health:grafana-alerts");
                assert!(!f.remediable, "observe-only in M0");
                assert!(f.detail.contains('3'));
            }
        }
    }

    /// THE distinction this janitor exists for. Two lanes, identical
    /// `status`, identical failing-poll streak — one has never once
    /// succeeded, the other worked five minutes ago. Only the first earns a
    /// row.
    ///
    /// The second half is the load-bearing half: it flips the SAME source
    /// from Degraded to Blind with its streak already past the bar, and the
    /// row appears. That proves the gate is the VERDICT and not the streak
    /// — a test that only checked "blind fires" would still pass if the
    /// verdict check were deleted.
    #[test]
    fn only_a_blind_source_files_a_row_never_a_degraded_one() {
        let env = MockJanitorEnv::default();
        let mut j = SuggestHealthJanitor::new(3);
        for poll_ms in [10_000_u64, 20_000, 30_000] {
            *env.health.lock().unwrap() = vec![
                // Never once succeeded → BLIND.
                (SourceKind::GrafanaAlerts, health_row(SourceStatus::Error, poll_ms)),
                // Observed its upstream at t=500, failing since → DEGRADED.
                (
                    SourceKind::JiraSprint,
                    degraded_row(SourceStatus::Error, poll_ms, 500),
                ),
            ];
            let findings = j.observe(&env, poll_ms + 500);
            if poll_ms < 30_000 {
                assert!(findings.is_empty(), "below the bar, both stay silent");
                continue;
            }
            // At the bar: exactly ONE row, and it is the blind lane.
            assert_eq!(findings.len(), 1, "a degraded lane must not file a row");
            let f = &findings[0];
            assert_eq!(f.key, "janitor:suggest-health:grafana-alerts");
            // The row says it in words, not as a status label to decode.
            assert!(
                f.title.contains("never once succeeded"),
                "title must name the fact plainly: {}",
                f.title
            );
            assert!(
                f.detail.contains("a declaration to fix, not an outage to wait out"),
                "detail must say it is not weather: {}",
                f.detail
            );
            // Blind needs intervention — it outranks the old status scale,
            // which sent this case (AuthMissing/Error, never-ok) to Low.
            assert_eq!(f.urgency, Urgency::High);
            assert!(!f.remediable, "observe-only in M0");
        }
        // Same jira lane, streak already well past the bar, now flipped to
        // "never succeeded": the row appears purely because the VERDICT
        // changed.
        *env.health.lock().unwrap() =
            vec![(SourceKind::JiraSprint, health_row(SourceStatus::Error, 40_000))];
        let findings = j.observe(&env, 40_500);
        assert_eq!(findings.len(), 1, "verdict flip alone must produce the row");
        assert_eq!(findings[0].key, "janitor:suggest-health:jira-sprint");
    }

    /// The literal line the operator reads on Ctrl-S. Pinned, because the
    /// whole point of this janitor is the WORDING: a row that said
    /// "erroring" is what hid the class, and a future edit that quietly
    /// reverts to a status label would restore the bug with every test
    /// still green.
    #[test]
    fn the_blind_row_renders_the_fact_in_words_on_the_board() {
        let env = MockJanitorEnv::default();
        let mut j = SuggestHealthJanitor::new(1);
        *env.health.lock().unwrap() =
            vec![(SourceKind::GrafanaAlerts, health_row(SourceStatus::Error, 10_000))];
        let findings = j.observe(&env, 10_500);
        let f = &findings[0];
        // Render exactly as the picker does: the ○ latent badge, then the
        // izumi board-row label of the agent-lane row `project_to_board`
        // injects (`<emoji> <title>  <detail>`).
        let spawn = crate::suggest::SpawnSpec::new("/x", "n").expect("spawn");
        let row = crate::suggest::Suggestion::new(
            crate::suggest::SourceKind::Agent,
            &f.key,
            &f.title,
            spawn,
        )
        .detail(f.detail.clone());
        let mut label = String::from("\u{25cb} ");
        label.push_str(&row.picker_label().to_string());
        assert_eq!(
            label,
            "\u{25cb} \u{1F91D} blind source: grafana-alerts has never once succeeded  \
             status \u{201C}erroring\u{201D} for 1 poll and not one success \
             \u{2014} a declaration to fix, not an outage to wait out"
        );
    }

    /// At most ONE row per blind source, however long it stays blind: the
    /// key is stable, so re-projection is an idempotent upsert rather than
    /// a growing pile.
    #[test]
    fn a_blind_source_files_one_row_not_one_per_poll() {
        let env = MockJanitorEnv::default();
        let mut j = SuggestHealthJanitor::new(1);
        let mut keys: Vec<String> = Vec::new();
        for poll_ms in [10_000_u64, 20_000, 30_000, 40_000] {
            *env.health.lock().unwrap() =
                vec![(SourceKind::GrafanaAlerts, health_row(SourceStatus::Error, poll_ms))];
            for f in j.observe(&env, poll_ms + 500) {
                keys.push(f.key);
            }
        }
        assert_eq!(keys.len(), 4, "one finding per poll…");
        keys.dedup();
        assert_eq!(keys.len(), 1, "…all carrying ONE stable board-row identity");
    }

    #[test]
    fn stalled_poll_clock_does_not_accumulate_and_recovery_resets() {
        let env = MockJanitorEnv::default();
        let mut j = SuggestHealthJanitor::new(2);
        // Two janitor ticks over the SAME poll: counts once, not twice.
        *env.health.lock().unwrap() =
            vec![(SourceKind::TendRepos, health_row(SourceStatus::AuthMissing, 10_000))];
        assert!(j.observe(&env, 11_000).is_empty());
        assert!(j.observe(&env, 12_000).is_empty(), "same poll must not re-count");
        // A recovery poll resets the streak entirely.
        *env.health.lock().unwrap() =
            vec![(SourceKind::TendRepos, health_row(SourceStatus::Ok, 20_000))];
        assert!(j.observe(&env, 21_000).is_empty());
        *env.health.lock().unwrap() =
            vec![(SourceKind::TendRepos, health_row(SourceStatus::AuthMissing, 30_000))];
        assert!(j.observe(&env, 31_000).is_empty(), "streak restarted at 1");
    }

    // ── Runner: shadow gate + cadence + board projection ───────────

    fn armed_config(authority: Authority) -> JanitorsConfig {
        JanitorsConfig {
            enabled: true,
            authority,
            board_rows: true,
            ghost_session: GhostSessionJanitorConfig {
                enabled: true,
                interval_secs: 60,
                grace_secs: 0,
                authority: None,
            },
            suggest_health: SuggestHealthJanitorConfig {
                enabled: true,
                interval_secs: 60,
                min_consecutive_polls: 3,
                authority: None,
            },
        }
    }

    #[test]
    fn shadow_mode_publishes_findings_but_never_writes() {
        let env = MockJanitorEnv::with_sessions(vec![ghost_view("aaaa")]);
        let bus = FiberBus::default();
        let mut findings_rx = bus.subscribe(Subject::Janitors);
        let mut sessions_rx = bus.subscribe(Subject::Sessions);
        let mut runner = JanitorRunner::from_config(&armed_config(Authority::Shadow));
        runner.tick(&env, &bus, 1_000);       // anchors the grace clock
        runner.tick(&env, &bus, 1_000 + 61_000); // grace 0 ⇒ finding fires
        // The finding was published, disposition ShadowHeld.
        let mut saw_shadow_held = false;
        while let Ok(ev) = findings_rx.try_recv() {
            if let FiberEvent::Janitor(f) = ev {
                assert_ne!(f.outcome, RemediationOutcome::Applied, "shadow must not apply");
                if f.outcome == RemediationOutcome::ShadowHeld {
                    saw_shadow_held = true;
                }
            }
        }
        assert!(saw_shadow_held, "remediable finding must be shadow-held");
        // No write happened: no close, no session event, session intact.
        assert!(env.closes().is_empty(), "shadow mode must never close");
        assert!(matches!(sessions_rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(env.tear_sessions().len(), 1);
        // Board projection still ran (findings surface in every mode).
        assert!(env.projected().contains(&String::from("janitor:ghost-session:aaaa")));
    }

    #[test]
    fn effect_mode_remediates_through_the_guarded_close() {
        let env = MockJanitorEnv::with_sessions(vec![ghost_view("aaaa")]);
        let bus = FiberBus::default();
        let mut findings_rx = bus.subscribe(Subject::Janitors);
        let mut sessions_rx = bus.subscribe(Subject::Sessions);
        let mut runner = JanitorRunner::from_config(&armed_config(Authority::Effect));
        runner.tick(&env, &bus, 1_000);
        runner.tick(&env, &bus, 1_000 + 61_000);
        assert_eq!(env.closes(), vec![String::from("aaaa")]);
        assert!(env.tear_sessions().is_empty(), "ghost closed");
        // The Applied disposition rode the finding…
        let mut saw_applied = false;
        while let Ok(FiberEvent::Janitor(f)) = findings_rx.try_recv() {
            if f.outcome == RemediationOutcome::Applied {
                saw_applied = true;
            }
        }
        assert!(saw_applied);
        // …and the reap published a session-lifecycle event.
        match sessions_rx.try_recv() {
            Ok(FiberEvent::Session(SessionEvent::GhostSessionReaped { session_id })) => {
                assert_eq!(session_id, "aaaa");
            }
            other => panic!("expected GhostSessionReaped, got {other:?}"),
        }
    }

    #[test]
    fn per_janitor_authority_overrides_the_global() {
        let mut cfg = armed_config(Authority::Shadow);
        cfg.ghost_session.authority = Some(Authority::Effect);
        cfg.ghost_session.grace_secs = 0;
        let env = MockJanitorEnv::with_sessions(vec![ghost_view("aaaa")]);
        let bus = FiberBus::default();
        let mut runner = JanitorRunner::from_config(&cfg);
        runner.tick(&env, &bus, 1_000);
        runner.tick(&env, &bus, 1_000 + 61_000);
        assert_eq!(env.closes(), vec![String::from("aaaa")], "override must win");
    }

    #[test]
    fn interval_gates_reobservation_and_disabled_plane_is_inert() {
        let env = MockJanitorEnv::with_sessions(vec![ghost_view("aaaa")]);
        let bus = FiberBus::default();
        let mut runner = JanitorRunner::from_config(&armed_config(Authority::Shadow));
        runner.tick(&env, &bus, 1_000);
        // 30s later: inside the 60s interval — the janitor must NOT re-run
        // (the grace clock stays anchored at the first tick).
        let processed = runner.tick(&env, &bus, 31_000);
        assert_eq!(processed, 0, "interval must gate the re-run");
        // Disabled plane: no slots, ticks are no-ops.
        let off = JanitorsConfig {
            enabled: false,
            ..armed_config(Authority::Effect)
        };
        let mut inert = JanitorRunner::from_config(&off);
        assert!(!inert.is_active());
        assert_eq!(inert.tick(&env, &bus, 999_000), 0);
        assert!(env.closes().is_empty());
    }

    #[test]
    fn board_projection_is_config_gated() {
        let mut cfg = armed_config(Authority::Shadow);
        cfg.board_rows = false;
        let env = MockJanitorEnv::with_sessions(vec![ghost_view("aaaa")]);
        let bus = FiberBus::default();
        let mut board_rx = bus.subscribe(Subject::Board);
        let mut runner = JanitorRunner::from_config(&cfg);
        runner.tick(&env, &bus, 1_000);
        runner.tick(&env, &bus, 1_000 + 61_000);
        assert!(env.projected().is_empty(), "board_rows=false must not inject");
        assert!(matches!(board_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    // ── Config tiers ───────────────────────────────────────────────

    #[test]
    fn config_tiers_bare_off_prescribed_shadow_on() {
        let bare = JanitorsConfig::bare();
        assert!(!bare.enabled);
        assert!(!JanitorRunner::from_config(&bare).is_active());
        let prescribed = JanitorsConfig::prescribed();
        assert!(prescribed.enabled);
        assert_eq!(prescribed.authority, Authority::Shadow, "shadow-first");
        assert!(prescribed.board_rows);
        assert!(prescribed.ghost_session.enabled);
        assert!(prescribed.suggest_health.enabled);
        assert_eq!(prescribed, JanitorsConfig::default());
        let runner = JanitorRunner::from_config(&prescribed);
        assert!(runner.is_active());
    }

    #[test]
    fn config_rejects_unknown_keys_and_round_trips_authority() {
        // deny_unknown_fields: a typo'd key is a hard parse error.
        let err = serde_yaml_ng::from_str::<JanitorsConfig>("enabled: true\nauthorty: effect\n");
        assert!(err.is_err(), "typo'd key must be rejected");
        // The kebab authority wire form round-trips.
        let cfg: JanitorsConfig = serde_yaml_ng::from_str(
            "enabled: true\nauthority: effect\nghost_session:\n  enabled: true\n  authority: shadow\n",
        )
        .expect("valid section parses");
        assert_eq!(cfg.authority, Authority::Effect);
        assert_eq!(cfg.ghost_session.authority, Some(Authority::Shadow));
        let back = serde_yaml_ng::to_string(&cfg).expect("serializes");
        assert!(back.contains("authority: effect"));
    }
}
