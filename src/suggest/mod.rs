//! The mado **suggestion stream** — a continuously-refreshing, gently
//! shaded-in stack of "what you could start working on right now" the Ctrl-S
//! session picker surfaces beneath the live + preset rows.
//!
//! ## Shape (the destination, named first)
//!
//! Open Ctrl-S any time and the lower band of the picker is a living,
//! self-refreshing list of task pointers — a PR awaiting your review, a sprint
//! ticket, a failing Flux/k8s resource, a dirty repo, a Cursor agent needing
//! follow-up, an incident firing — each ranked by urgency + score, each
//! **Enter-spawns a pre-named session aimed at that task**. Sources are
//! shikumi config; adding one is one `izumi::Source` impl. Nothing dead
//! or duplicate is ever offered. The terminal proposes the next task before
//! you go looking — flow dominance.
//!
//! ## Layering (reuse-first, no-overlap)
//!
//! The plane's algebra lives in the **izumi** substrate (the generalized
//! extraction of this module): `izumi` (Item/Store/Engine/Environment),
//! `izumi-sources` (the 25 generic providers), `izumi-config` (the shikumi
//! `BoardConfig` surface). This module is the mado-side SHIM: it declares the
//! 29-variant [`SourceKind`] catalog, re-exports the izumi types under their
//! historical mado names (submodule paths preserved for every importer), keeps
//! the mado-local providers ([`sources`]: recent-dirs, project-marks, agent
//! lane, safra adapter), and owns the process-global engine/board facade.
//!
//! Wire compatibility is contractual: the snapshot magic
//! [`store::SNAPSHOT_MAGIC`] (`b"mado-suggest v1\n"`, trailing newline
//! INCLUDED) is passed to izumi's magic-parameterized persist layer verbatim,
//! item ids derive byte-identically (fnv1a over `slug ':' key`), and the
//! `suggestions:` YAML schema is unchanged.

// The suggestion plane is an actively-built substrate the binary consumes
// thinly: several typed-API items are exercised by the source providers +
// later milestones and by the per-module test suites, but not yet by a
// non-test build of the binary. Allow dead_code module-wide while the plane
// fills out rather than scatter per-item allows that churn as each milestone
// lands.
#![allow(dead_code)]

pub mod sources;

/// The typed suggestion values — shim over `izumi`'s item plane, preserving
/// the historical `crate::suggest::core::*` import paths. [`SourceKind`] (the
/// mado catalog) is authored here via [`izumi::catalog!`].
pub mod core {
    #[allow(unused_imports)]
    pub use izumi::{CorrKey, Rank, SourceStatus, SpawnSpec, Urgency, fnv1a};

    /// Stable identity of a suggestion — izumi's [`izumi::ItemId`] under the
    /// historical mado name (the tuple constructor `SuggestionId(raw)` still
    /// works: a `pub use` rename carries the value namespace).
    pub use izumi::ItemId as SuggestionId;

    /// One latent task the picker can shade in + spawn — izumi's
    /// [`izumi::Item`] over the mado catalog + spawn payload.
    pub type Suggestion = izumi::Item<SourceKind, SpawnSpec>;

    izumi::catalog! {
        /// Every task-suggestion source the plane knows about — the COMPLETE
        /// catalog (CATALOG REFLECTION). Declared via `izumi::catalog!`, so
        /// slugs are compile-time unique, `Ord` is declaration order, and the
        /// serde wire form is the kebab slug — byte-identical to the previous
        /// hand-written `#[serde(rename_all = "kebab-case")]` enum.
        pub enum SourceKind {
            // ── local / zero-auth (the proof-of-thread tier) ──────────────
            /// Local git branches correlated to their open PR titles.
            GitBranchPr { slug: "git-branch-pr", emoji: "\u{1F33F}", label: "git branch ↔ PR", urgency: Low, needs_auth: false, interval_secs: 30 },
            /// `tend` workspace repos that are dirty / unsynced / missing.
            TendRepos { slug: "tend-repos", emoji: "\u{1F9F9}", label: "tend dirty repos", urgency: Low, needs_auth: false, interval_secs: 30 },
            /// Recently-visited directories (mado's own recent-dirs).
            RecentDirs { slug: "recent-dirs", emoji: "\u{1F4C1}", label: "recent directories", urgency: Idle, needs_auth: false, interval_secs: 30 },
            /// User-set project marks (mado marks).
            ProjectMarks { slug: "project-marks", emoji: "\u{1F4CC}", label: "project marks", urgency: Idle, needs_auth: false, interval_secs: 30 },
            /// `cargo` warnings/errors in the current project.
            CargoWarnings { slug: "cargo-warnings", emoji: "\u{1F980}", label: "cargo warnings", urgency: Low, needs_auth: false, interval_secs: 120 },
            /// `TODO` / `FIXME` backlog under the code root.
            TodoBacklog { slug: "todo-backlog", emoji: "\u{1F4DD}", label: "TODO backlog", urgency: Idle, needs_auth: false, interval_secs: 120 },
            // ── github ────────────────────────────────────────────────────
            /// PRs awaiting your review.
            GithubReviewRequested { slug: "github-review-requested", emoji: "\u{1F50D}", label: "GitHub review-requested", urgency: High, needs_auth: true, interval_secs: 180 },
            /// Issues assigned to you.
            GithubAssignedIssues { slug: "github-assigned-issues", emoji: "\u{1F41B}", label: "GitHub assigned issues", urgency: Normal, needs_auth: true, interval_secs: 180 },
            /// GitHub Actions runs that are failing.
            GithubActionsFailing { slug: "github-actions-failing", emoji: "\u{1F6A8}", label: "GitHub Actions failing", urgency: High, needs_auth: true, interval_secs: 180 },
            // ── atlassian ─────────────────────────────────────────────────
            /// Jira issues in your active sprint.
            JiraSprint { slug: "jira-sprint", emoji: "\u{1F3AB}", label: "Jira sprint", urgency: Normal, needs_auth: true, interval_secs: 300 },
            /// Jira issues assigned to you.
            JiraAssigned { slug: "jira-assigned", emoji: "\u{1F4CB}", label: "Jira assigned", urgency: Normal, needs_auth: true, interval_secs: 300 },
            /// Confluence pages mentioning you.
            ConfluenceMentions { slug: "confluence-mentions", emoji: "\u{1F4AC}", label: "Confluence mentions", urgency: Low, needs_auth: true, interval_secs: 300 },
            // ── cluster / gitops ──────────────────────────────────────────
            /// FluxCD Kustomizations/HelmReleases failing to reconcile.
            FluxFailing { slug: "flux-failing", emoji: "\u{1F501}", label: "Flux failing", urgency: High, needs_auth: false, interval_secs: 60 },
            /// Kubernetes pods Pending / CrashLoopBackOff / unhealthy.
            K8sUnhealthy { slug: "k8s-unhealthy", emoji: "\u{2638}", label: "k8s unhealthy pods", urgency: Critical, needs_auth: false, interval_secs: 60 },
            /// `breathe` resource bands stuck in Conflict.
            BreatheConflict { slug: "breathe-conflict", emoji: "\u{1F4A8}", label: "breathe Conflict bands", urgency: High, needs_auth: false, interval_secs: 60 },
            /// engenho cluster nodes not Ready.
            EngenhoNodes { slug: "engenho-nodes", emoji: "\u{1F5A5}", label: "engenho nodes", urgency: Normal, needs_auth: false, interval_secs: 60 },
            // ── observability / incidents ─────────────────────────────────
            /// grafana alerts firing.
            GrafanaAlerts { slug: "grafana-alerts", emoji: "\u{1F525}", label: "grafana alerts", urgency: Critical, needs_auth: true, interval_secs: 90 },
            /// grafana incidents open.
            GrafanaIncidents { slug: "grafana-incidents", emoji: "\u{1F6A9}", label: "grafana incidents", urgency: Critical, needs_auth: true, interval_secs: 90 },
            /// grafana on-call shifts assigned to you.
            GrafanaOncall { slug: "grafana-oncall", emoji: "\u{1F4DF}", label: "grafana on-call", urgency: High, needs_auth: true, interval_secs: 600 },
            /// Datadog monitors alerting.
            DatadogMonitors { slug: "datadog-monitors", emoji: "\u{1F415}", label: "Datadog monitors", urgency: Critical, needs_auth: true, interval_secs: 90 },
            /// Opsgenie alerts open/unacked.
            OpsgenieAlerts { slug: "opsgenie-alerts", emoji: "\u{1F514}", label: "Opsgenie alerts", urgency: Critical, needs_auth: true, interval_secs: 90 },
            // ── agents / cloud ────────────────────────────────────────────
            /// Cursor cloud (kurage) agents needing follow-up.
            KurageAgents { slug: "kurage-agents", emoji: "\u{1F916}", label: "Cursor agents", urgency: Normal, needs_auth: true, interval_secs: 120 },
            /// AWS health/PHD events affecting your account.
            AwsHealth { slug: "aws-health", emoji: "\u{2601}", label: "AWS health", urgency: Critical, needs_auth: true, interval_secs: 300 },
            /// Cloudflare Pages/Workers deployments that failed.
            CloudflareDeployments { slug: "cloudflare-deployments", emoji: "\u{1F310}", label: "Cloudflare deployments", urgency: High, needs_auth: true, interval_secs: 300 },
            // ── calendar / tasks ──────────────────────────────────────────
            /// Google Tasks due soon.
            GoogleTasks { slug: "google-tasks", emoji: "\u{2705}", label: "Google Tasks", urgency: Low, needs_auth: true, interval_secs: 300 },
            /// Google Calendar events imminent.
            GoogleCalendar { slug: "google-calendar", emoji: "\u{1F4C5}", label: "Google Calendar", urgency: Normal, needs_auth: true, interval_secs: 300 },
            // ── secrets ───────────────────────────────────────────────────
            /// Secrets whose age exceeds a rotation threshold.
            SecretAge { slug: "secret-age", emoji: "\u{1F511}", label: "secret age", urgency: Normal, needs_auth: false, interval_secs: 3600 },
            // ── curated observability (safra) ─────────────────────────────
            /// The safra curation plane: per-(environment × data-kind) curated
            /// signals (firing alerts, deploy flows, SLO breaches) projected
            /// onto the board. Cells + endpoints come from the `safra:`
            /// config section.
            Safra { slug: "safra", emoji: "\u{1F33E}", label: "safra curated signals", urgency: Normal, needs_auth: false, interval_secs: 60 },
            // ── agent lane ────────────────────────────────────────────────
            /// Tasks PUSHED onto the board by an agent through the
            /// `suggest_inject` MCP tool (an agent needing review, a hand-off,
            /// a follow-up). Push-only: no watcher polls it; rows live until
            /// they decay or are dismissed.
            Agent { slug: "agent", emoji: "\u{1F91D}", label: "agent-injected tasks", urgency: Normal, needs_auth: false, interval_secs: 300 },
        }
    }

    /// Inherent mirrors of the [`izumi::Catalog`] table methods, so the
    /// historical `SourceKind::ALL` / `kind.slug()` / `kind.emoji()` call
    /// sites keep working WITHOUT importing the trait at every consumer.
    impl SourceKind {
        /// Every variant, in catalog (declaration) order — the reflection
        /// surface tooling + config + tests iterate.
        pub const ALL: &'static [SourceKind] = <SourceKind as izumi::Catalog>::ALL;

        /// Kebab slug — the stable id-derivation key + serde wire form.
        #[must_use]
        pub fn slug(self) -> &'static str {
            izumi::Catalog::slug(self)
        }

        /// Resolve a slug back to its kind (config parse / round-trip).
        #[must_use]
        pub fn from_slug(s: &str) -> Option<SourceKind> {
            <SourceKind as izumi::Catalog>::from_slug(s)
        }

        /// One-glyph emoji signal for the picker row.
        #[must_use]
        pub fn emoji(self) -> &'static str {
            izumi::Catalog::emoji(self)
        }

        /// Human label for config docs / tooling.
        #[must_use]
        pub fn label(self) -> &'static str {
            izumi::Catalog::label(self)
        }

        /// Default urgency a fresh suggestion from this source carries.
        #[must_use]
        pub fn default_urgency(self) -> Urgency {
            izumi::Catalog::default_urgency(self)
        }

        /// Whether the source needs a token/credential to return anything.
        #[must_use]
        pub fn needs_auth(self) -> bool {
            izumi::Catalog::needs_auth(self)
        }

        /// Default poll cadence in seconds.
        #[must_use]
        pub fn default_interval_secs(self) -> u64 {
            izumi::Catalog::default_interval_secs(self)
        }
    }
}

/// The mockable I/O boundary — shim over [`izumi::env`], preserving the
/// historical `crate::suggest::env::*` paths (the trait keeps its mado name
/// `SuggestionEnvironment`; a trait `pub use` rename is impl- and dyn-usable
/// because [`izumi::Environment`] carries no generic parameters).
pub mod env {
    pub use izumi::Environment as SuggestionEnvironment;
    #[allow(unused_imports)]
    pub use izumi::env::{Cmd, HttpReq, MockEnvironment, RealEnvironment};
}

/// The provider border + watcher engine — shim over [`izumi::source`] /
/// [`izumi::engine`], monomorphized to the mado catalog + spawn payload.
pub mod source {
    use super::core::{SourceKind, SpawnSpec};

    /// The typed outcome of one poll (mado monomorphization).
    pub type PollOutcome = izumi::PollOutcome<SourceKind, SpawnSpec>;
    /// Engine-wide config over the mado catalog.
    pub type EngineConfig = izumi::EngineConfig<SourceKind>;
    /// The parallel watcher engine (izumi's [`izumi::Engine`]; `start` now
    /// takes the freshness nudge as a PARAMETER — the facade passes
    /// [`super::board_nudge`]).
    pub use izumi::Engine as SuggestionEngine;
    #[allow(unused_imports)]
    pub use izumi::source::{SourceConfig, apply_poll, refresh_once};

    /// The object-safe provider type mado registries hold. NOTE: the generic
    /// [`izumi::Source`] trait cannot be `pub use`-renamed into an
    /// `impl SuggestionSource for X` position (E0107 — the type parameters
    /// don't default); providers write
    /// `impl izumi::Source<SourceKind, izumi::SpawnSpec> for X` directly.
    pub type DynSuggestionSource = dyn izumi::Source<SourceKind, SpawnSpec>;
}

/// The living-board store — shim over [`izumi::store`] monomorphized to the
/// mado catalog + spawn payload, plus the mado snapshot-magic constants and
/// the magic-hardwired framed-persist helpers `praca_store` shares.
pub mod store {
    use std::path::Path;

    use super::core::{SourceKind, SpawnSpec};

    /// Snapshot framing magic — the mado schema tag, passed VERBATIM
    /// (trailing newline INCLUDED) to izumi's magic-parameterized persist
    /// layer, so every pre-extraction snapshot still loads and every new
    /// write is byte-identical to the old frame.
    pub const SNAPSHOT_MAGIC: &[u8] = b"mado-suggest v1\n";

    /// The ranked living-board cache (mado monomorphization).
    pub type SuggestionStore = izumi::Store<SourceKind, SpawnSpec>;
    /// A suggestion plus store bookkeeping. The Rust field for the item is
    /// `item` (izumi's name); its serde wire name stays `suggestion`.
    pub type StoredSuggestion = izumi::StoredItem<SourceKind, SpawnSpec>;
    /// Serializable warm-restart snapshot view.
    pub type StoreSnapshot = izumi::StoreSnapshot<SourceKind, SpawnSpec>;
    /// Worked-on lifecycle state (wire form unchanged: kebab-case,
    /// `kind`-tagged).
    pub use izumi::ItemState as SuggestionState;
    #[allow(unused_imports)]
    pub use izumi::store::{
        SourceHealth, balance_per_source, collapse_correlated, effective_rank_key, shade_ramp,
    };

    /// Frame `json` under the mado magic and atomically write it to `path`
    /// (see [`izumi::persist::atomic_write_framed`]). The praça snapshot
    /// writes through this same magic-hardwired helper.
    pub fn atomic_write_framed(path: &Path, json: &[u8]) {
        izumi::persist::atomic_write_framed(SNAPSHOT_MAGIC, path, json);
    }

    /// Verify + unframe a mado-magic snapshot: `None` on wrong magic (schema
    /// bump) or hash mismatch (torn/corrupt) — both mean start-empty.
    #[must_use]
    pub fn unframe_snapshot(bytes: &[u8]) -> Option<Vec<u8>> {
        izumi::persist::unframe_snapshot(SNAPSHOT_MAGIC, bytes)
    }
}

// ─────────────────────────────────────────────────────────────────
// The health verdict — imported, no longer mirrored
// ─────────────────────────────────────────────────────────────────
//
// `status` answers "is this source erroring *right now*". That is the
// question that let seven sources sit dead for 118+ consecutive polls: a lane
// misconfigured the day it was wired reads `erroring`, which is exactly what a
// healthy lane reads during a transient upstream blip. Same word, opposite
// meaning — one is weather, the other is a build defect. The second axis was
// always in the data: has this source EVER observed its upstream?
//
// Mado used to answer that with its OWN `mod health` — a `HealthVerdict` enum
// plus a `verdict(&SourceHealth)` free function whose doc comment named
// `izumi_board::protocol::HealthVerdict` as a known duplicate and called one
// `verdict()` on `izumi::SourceHealth` the destination. izumi `c2b48c0` built
// that destination, so the copy is deleted rather than kept in sync: two
// copies of one rule WILL drift, and the drift is silent because each side
// looks locally correct. The rule now has exactly one home
// ([`izumi::HealthVerdict::of`]), and mado reads it through
// `SourceHealth::verdict()` like every other border.

/// The typed success history behind a verdict — `Succeeded` / `NeverSucceeded`
/// / `Unobserved`. Re-exported so a mado-side border that DECIDES something
/// can read the evidence directly instead of collapsing it to a boolean;
/// `ever_ok()` cannot tell "we looked and it never worked" from "we never
/// looked", and a gate that cannot tell those apart fires on the wrong one.
#[allow(unused_imports)]
pub use izumi::OkEvidence;

/// Whether a source is reporting, failing, has never worked at all, or has
/// never been looked at — **izumi's definition, not a mado copy**.
///
/// **The fourth variant is the point.** [`HealthVerdict::Unknown`] (no poll
/// has ever completed) is NOT [`HealthVerdict::Blind`] (polled, never once
/// observed to succeed). Blind is a finding; unknown is the absence of one,
/// and only a finding may gate — [`HealthVerdict::needs_intervention`] is
/// true for Blind and nothing else. Every mado consumer matches all four
/// arms explicitly, so folding Unknown back into Blind has to be typed out
/// on purpose rather than happening by default.
///
/// **Tier-honest — what `min_consecutive_polls` is now for.** The old note
/// here said this classification was `only-mitigated` because `last_ok_ms`
/// was per-PROCESS: `StoreSnapshot` persisted `entries` + `saved_at_ms` and
/// dropped `health`, so every mado restart reset the success latch and a
/// merely-degraded source read `Blind` until its next good poll. The janitor's
/// `min_consecutive_polls` bar was the mitigation for exactly that, which is
/// why it was documented as load-bearing rather than as a tuning knob.
///
/// izumi `c2b48c0` fixed it at the cause: the snapshot carries the health
/// plane (slug-keyed, `#[serde(default)]`, merged monotonically), so the latch
/// and the observation window are facts about the SOURCE, not about this
/// process. `min_consecutive_polls` therefore reverts to what its name always
/// said — debouncing a flapping upstream so one bad poll cannot file a row.
/// Setting it to 1 is now a noise choice, not a correctness risk.
///
/// **The honest residue is the one-time transition, and only that.** A
/// snapshot written before the health plane existed carries none, so the first
/// process after the upgrade starts with no record for any source. That state
/// is typed [`OkEvidence::Unobserved`] → [`HealthVerdict::Unknown`], never a
/// false `NeverSucceeded`, so it reads as *unknown* and files nothing; from
/// the second run on there is nothing left to narrow. The verdict is an
/// eval-time derived value, not a compile error — what IS unrepresentable is
/// the FORK: a border cannot re-decide the rule without deleting
/// `HealthVerdict::of`, and a new `SourceStatus` or `OkEvidence` variant is a
/// compile error at that one site.
pub use izumi::HealthVerdict;

// The suggest-plane facade. These are the module's public API; not every name
// is consumed by the binary itself (several are used cross-module only under
// cfg(test) or by providers via their full paths), so the unused-import lint
// for the re-export surface is intentionally allowed.
#[allow(unused_imports)]
pub use core::{CorrKey, SourceKind, SourceStatus, SpawnSpec, Suggestion, SuggestionId, Urgency};
#[allow(unused_imports)]
pub use env::{Cmd, HttpReq, MockEnvironment, RealEnvironment, SuggestionEnvironment};
#[allow(unused_imports)]
pub use source::{
    DynSuggestionSource, EngineConfig, PollOutcome, SourceConfig, SuggestionEngine, refresh_once,
};
#[allow(unused_imports)]
pub use store::{
    SourceHealth, StoreSnapshot, StoredSuggestion, SuggestionState, SuggestionStore, shade_ramp,
};

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// The process-wide suggestion cache — created lazily on first access. The
/// watcher engine (started on its own runtime thread, like vigy) fills it; the
/// Ctrl-S picker bridge reads it. One store, many readers — never a fork.
static STORE: OnceLock<Arc<SuggestionStore>> = OnceLock::new();

/// The shared suggestion store handle (lazy global). Always available; empty
/// until the engine runs (so a disabled engine simply yields no rows).
#[must_use]
pub fn store() -> Arc<SuggestionStore> {
    Arc::clone(STORE.get_or_init(|| Arc::new(SuggestionStore::new())))
}

/// The process-wide freshness nudge every watcher selects on alongside its
/// cadence tick (see `izumi::refresh::spawn_interval_refresh_nudged`).
static NUDGE: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();

/// The shared board-refresh notify (lazy global) — watchers wait on it, the
/// GUI fires it.
#[must_use]
pub fn board_nudge() -> Arc<tokio::sync::Notify> {
    Arc::clone(NUDGE.get_or_init(|| Arc::new(tokio::sync::Notify::new())))
}

/// Ask every suggestion watcher to refresh EARLY (paced per-watcher — a
/// watcher inside its pacing gap absorbs the nudge). Fired when the Ctrl-S
/// board opens, so the operator opens onto data being re-verified right now
/// instead of whenever the next interval happens to land. Sync-callable from
/// the GUI thread; wakes only parked watchers, never blocks.
pub fn request_board_refresh() {
    board_nudge().notify_waiters();
}

// ── Engine hot-reload control plane ─────────────────────────────────────

/// Command ingress for the engine maintenance loop. Boxed pair keeps the
/// channel payload one pointer wide.
enum EngineCommand {
    /// Tear down the running engine (if any) and rebuild it from the new
    /// `suggestions` + `safra` + `janitors` config sections. `enabled =
    /// false` parks the loop with no engine; `enabled = true` (re)builds —
    /// enable, disable, and reconfigure are ONE uniform path. The janitor
    /// runner rebuilds on the same swap, so the janitor plane hot-reloads
    /// with zero extra machinery.
    Swap(
        Box<(
            crate::config::SuggestionsConfig,
            crate::safra::SafraConfig,
            crate::config::JanitorsConfig,
        )>,
    ),
}

/// Handle for requesting an engine rebuild from the GUI render thread.
/// `UnboundedSender::send` is sync-callable — no runtime handle needed at
/// the call site.
pub struct EngineControl {
    tx: tokio::sync::mpsc::UnboundedSender<EngineCommand>,
}

impl EngineControl {
    /// Request a rebuild against the given config sections. Idempotent at
    /// the receiver (a swap equal to the running config is dropped), so
    /// double-fire from the two render adapters is harmless.
    pub fn swap(
        &self,
        suggestions: crate::config::SuggestionsConfig,
        safra: crate::safra::SafraConfig,
        janitors: crate::config::JanitorsConfig,
    ) {
        let _ = self.tx.send(EngineCommand::Swap(Box::new((
            suggestions,
            safra,
            janitors,
        ))));
    }
}

/// Registered once by `spawn_engine_thread`; holding the sender in a static
/// keeps the control channel open for the process lifetime (recv can never
/// yield `None` while the static lives).
static ENGINE_CONTROL: OnceLock<EngineControl> = OnceLock::new();

/// The engine control handle — `None` only before `spawn_engine_thread`
/// ran (the thread now spawns unconditionally, so after boot this is
/// always `Some` and a boot-disabled engine can still be hot-enabled).
#[must_use]
pub fn engine_control() -> Option<&'static EngineControl> {
    ENGINE_CONTROL.get()
}

// ── The agent-facing board surface (shared by two ingresses) ────────────
//
// `mado mcp` is a SEPARATE process from the GUI, so its process-global
// store is NOT the operator's live board. The three functions below are the
// ONE implementation both ingresses call: the GUI's kanshou leaves (the
// live-board truth an MCP tool forwards to — the `list_sessions` idiom) and
// the MCP tools' local fallback (headless mado, no GUI running).

/// Wall-clock unix milliseconds — the board surface's single clock read.
#[must_use]
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// JSON view of THIS process's living board: ranked offerable rows (id,
/// source, title, urgency, lifecycle state, recurrence, spawn target) plus
/// per-source poll health. Ids are DECIMAL STRINGS — a u64 does not survive
/// JSON number precision.
#[must_use]
pub fn board_json(max: usize) -> serde_json::Value {
    let store = store();
    let now_ms = now_unix_ms();
    let rows: Vec<serde_json::Value> = store
        .ranked_stored(max.clamp(1, 200), now_ms)
        .into_iter()
        .map(|st| {
            serde_json::json!({
                "id": st.item.id.0.to_string(),
                "source": st.item.source.slug(),
                "title": st.item.title,
                "detail": st.item.detail,
                "urgency": serde_json::to_value(st.item.urgency)
                    .unwrap_or(serde_json::Value::Null),
                "state": serde_json::to_value(&st.state)
                    .unwrap_or(serde_json::Value::Null),
                "times_seen": st.times_seen,
                "waiting_secs": now_ms.saturating_sub(st.first_seen_ms) / 1000,
                "cwd": st.item.spawn.cwd().to_string_lossy(),
                "session_name": st.item.spawn.name(),
                "initial_command": st.item.spawn.initial_command(),
            })
        })
        .collect();
    let health: Vec<serde_json::Value> = store
        .health()
        .into_iter()
        .map(|(kind, h)| {
            // `ever_ok` stays (wire compat), but it is no longer the only
            // carrier of the fact: the VERDICT ships alongside it, from the
            // same definition the picker footer and the janitor row read —
            // so the agent face and the operator face can never disagree
            // about which lanes have never once succeeded. It is a DISPLAY
            // field now; anything deciding reads `ok_evidence()`, which can
            // say "never looked" where a boolean can only say `false`.
            let v = h.verdict();
            serde_json::json!({
                "source": kind.slug(),
                "status": h.status.label(),
                "last_poll_secs_ago": now_ms.saturating_sub(h.last_poll_ms) / 1000,
                "ever_ok": h.ever_ok(),
                // `.slug()`, NOT `.label()`. izumi's `label()` is the
                // fixed-width board form (`"BLIND   "`) built to scan
                // preattentively in a column; the wire wants the compact
                // machine token, which is what mado's own retired `label()`
                // happened to emit. Same four strings as before —
                // `ok`/`degraded`/`blind` — plus `unknown`, newly reachable
                // for a lane whose instrument has not run (it used to
                // mis-report as `blind`).
                "verdict": v.slug(),
                "needs_intervention": v.needs_intervention(),
            })
        })
        .collect();
    serde_json::json!({ "suggestions": rows, "health": health })
}

/// Typed parameters for an agent-injected board row — deserialized from the
/// MCP tool input AND from the kanshou call leaf's JSON argument (one
/// border, two ingresses).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct InjectParams {
    /// The task line the board shows (required, non-blank).
    pub title: String,
    /// Stable dedup key — re-injecting the same key updates the row.
    /// Defaults to the title.
    #[serde(default)]
    pub key: Option<String>,
    /// Secondary context shown dimmer.
    #[serde(default)]
    pub detail: Option<String>,
    /// idle | low | normal | high | critical. Default normal.
    #[serde(default)]
    pub urgency: Option<String>,
    /// Working directory the accepted session spawns into. Defaults to the
    /// operator's code root.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Session name for the accepted row. Defaults to `🤝 <title…>`.
    #[serde(default)]
    pub session_name: Option<String>,
    /// Kickoff command typed into the fresh session (control bytes rejected
    /// at the typed border).
    #[serde(default)]
    pub command: Option<String>,
}

/// Push a task onto THIS process's board (the 🤝 agent lane, additive
/// upsert). Returns the row id as a decimal string.
pub fn inject(params: InjectParams) -> Result<serde_json::Value, String> {
    let title = params.title.trim().to_owned();
    if title.is_empty() {
        return Err(String::from("title must be non-empty"));
    }
    let cwd = params
        .cwd
        .filter(|c| !c.trim().is_empty())
        .unwrap_or_else(|| {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            std::env::var_os("PLEME_CODE_ROOT")
                .map_or_else(|| home.join("code"), PathBuf::from)
                .to_string_lossy()
                .into_owned()
        });
    let name = params
        .session_name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| {
            let mut n = String::from("\u{1F91D} "); // 🤝 agent-lane native name
            n.push_str(title.chars().take(40).collect::<String>().trim());
            n
        });
    let Some(mut spawn) = SpawnSpec::new(cwd, name) else {
        return Err(String::from("cwd and session_name must be non-empty"));
    };
    if let Some(cmd) = params.command {
        spawn = spawn.with_command(cmd);
    }
    let key = params.key.unwrap_or_else(|| title.clone());
    let mut s = Suggestion::new(SourceKind::Agent, &key, &title, spawn);
    if let Some(d) = params.detail {
        s = s.detail(d);
    }
    if let Some(u) = params.urgency {
        match serde_json::from_value::<Urgency>(serde_json::Value::String(u)) {
            Ok(parsed) => s = s.urgent(parsed),
            Err(_) => {
                return Err(String::from(
                    "urgency must be idle|low|normal|high|critical",
                ));
            }
        }
    }
    let id = s.id;
    store().upsert(s, now_unix_ms());
    Ok(serde_json::json!({ "id": id.0.to_string() }))
}

/// Dismiss (or snooze) a board row by its decimal-string id on THIS
/// process's board.
pub fn dismiss(id_str: &str, snooze_secs: Option<u64>) -> Result<serde_json::Value, String> {
    let Ok(raw) = id_str.parse::<u64>() else {
        return Err(String::from(
            "id must be the decimal u64 string from the board list",
        ));
    };
    let id = core::SuggestionId(raw);
    let st = store();
    let done = match snooze_secs {
        Some(secs) => st.snooze(id, now_unix_ms().saturating_add(secs.saturating_mul(1000))),
        None => st.dismiss(id),
    };
    if done {
        Ok(serde_json::json!({ "dismissed": true, "id": id_str }))
    } else {
        Err(String::from(
            "no suggestion with that id (it may have decayed)",
        ))
    }
}

/// Resolve the warm-restart snapshot path. `$MADO_SUGGEST_DB` is an explicit
/// override; else `$MADO_STATE_DIR` (tests inject a temp dir); else the OS
/// state dir (`~/.local/state` on Linux — warm-restart data is operator-
/// meaningful *state*, not throwaway cache), falling back to the data dir, then
/// the temp dir. The file lives under a `mado/` subdir.
#[must_use]
pub fn state_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MADO_SUGGEST_DB") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("MADO_STATE_DIR")
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .or_else(dirs::data_dir)
        .unwrap_or_else(std::env::temp_dir);
    base.join("mado").join("suggestions.json")
}

/// Translate the typed shikumi `suggestions` config into an [`EngineConfig`].
/// Reads the MERGED source list (prescribed arm-list ⊕ operator overrides), so
/// a params-only yaml override never disarms the rest of the surface.
///
/// Kept LOCAL (not delegated to `izumi_config::to_engine_config`) because the
/// mado prescribed arm-list lives inside `SuggestionsConfig::prescribed()`
/// (YAML byte-compat is contractual); a parity test in `config.rs` proves the
/// merge semantics equal `izumi_config::BoardConfig::effective_sources`.
#[must_use]
pub fn engine_config_from(cfg: &crate::config::SuggestionsConfig) -> EngineConfig {
    let mut ec = EngineConfig {
        per_source: std::collections::BTreeMap::new(),
        default_enabled: cfg.default_enabled,
    };
    for s in &cfg.effective_sources() {
        if let Some(kind) = SourceKind::from_slug(&s.kind) {
            let mut sc = SourceConfig::for_kind(kind);
            sc.enabled = s.enabled;
            if let Some(iv) = s.interval_secs {
                sc.interval = std::time::Duration::from_secs(iv.max(1));
            }
            if let Some(mx) = s.max_items {
                sc.max_items = mx;
            }
            sc.params = s.params.clone();
            ec.per_source.insert(kind, sc);
        }
    }
    ec
}

/// Config-derived maintenance knobs — rebuilt on every hot swap so TTLs,
/// persistence cadence, and entry caps track the LIVE config, not the boot
/// config.
struct LoopKnobs {
    /// Per-source TTL: a source's items live for max(3× its poll interval,
    /// the global floor) — so a slow (e.g. hourly) source never flickers
    /// under a fast global TTL.
    ttl_map: std::collections::BTreeMap<SourceKind, u64>,
    global_ttl_ms: u64,
    /// WRITING is additionally re-decided every maintenance tick via the
    /// single-writer election; this knob is the config half of that AND.
    persist: bool,
    max_entries: usize,
    /// 0 = "persist on every change" → a 1s minimum tick (tokio rejects a
    /// 0 interval); otherwise coalesce writes to this cadence.
    debounce: std::time::Duration,
}

impl LoopKnobs {
    fn from_config(cfg: &crate::config::SuggestionsConfig, engine_cfg: &EngineConfig) -> Self {
        let global_ttl_ms = cfg.ttl_secs.saturating_mul(1000);
        let mut ttl_map: std::collections::BTreeMap<SourceKind, u64> =
            std::collections::BTreeMap::new();
        for &kind in SourceKind::ALL {
            let interval_ttl = engine_cfg
                .config_for(kind)
                .interval
                .as_secs()
                .saturating_mul(3)
                .saturating_mul(1000);
            ttl_map.insert(kind, interval_ttl.max(global_ttl_ms));
        }
        Self {
            ttl_map,
            global_ttl_ms,
            persist: cfg.persist,
            max_entries: cfg.max_entries,
            debounce: std::time::Duration::from_secs(cfg.persist_debounce_secs.max(1)),
        }
    }
}

/// Build a RUNNING engine from the live config sections: merged source
/// registry (with the safra adapter swapped in when its section declares
/// cells) + watcher start. The one boot/hot-swap construction path.
fn build_engine(
    sugg: &crate::config::SuggestionsConfig,
    safra: &crate::safra::SafraConfig,
    env: &Arc<dyn SuggestionEnvironment>,
    store: &Arc<SuggestionStore>,
) -> (SuggestionEngine, LoopKnobs) {
    let engine_cfg = engine_config_from(sugg);
    let knobs = LoopKnobs::from_config(sugg, &engine_cfg);
    // The safra plane: swap the registry's unconfigured placeholder for the
    // config-built adapter when the operator's safra: section declares cells.
    let mut sources_vec = sources::registry();
    if safra.enabled {
        let adapter = crate::safra::SafraSuggestionSource::from_config(safra);
        tracing::info!(cells = adapter.cell_count(), "safra plane live");
        sources_vec.retain(|s| s.kind() != SourceKind::Safra);
        sources_vec.push(Arc::new(adapter));
    }
    // izumi's Engine takes the freshness nudge as a PARAMETER (the extraction
    // decoupled it from mado's process-global); the shared board nudge keeps
    // the exact pre-extraction semantics.
    let engine = SuggestionEngine::start(
        sources_vec,
        Arc::clone(env),
        Arc::clone(store),
        engine_cfg,
        Some(board_nudge()),
    );
    tracing::info!(
        watchers = engine.active_watchers(),
        persist = knobs.persist,
        "mado suggestion engine live"
    );
    (engine, knobs)
}

/// Spawn the suggestion control plane on its own multi-thread tokio runtime
/// thread — mirrors the vigy runtime (the GUI thread is not async). Spawns
/// UNCONDITIONALLY: the `suggestions.enabled` gate lives INSIDE the thread
/// (a boot-disabled engine is a parked loop holding the control channel, so
/// a later config edit can hot-enable it — enable/disable/reconfigure are
/// one uniform [`EngineCommand::Swap`] path via [`engine_control`]).
/// Best-effort (a runtime build failure logs + leaves the store empty, so
/// the picker simply shows no suggestions). Parking the disabled loop also
/// keeps `praca_store::maintenance_tick` alive, so praça preset persistence
/// no longer depends on suggestions being enabled.
#[allow(clippy::too_many_lines)] // one linear boot+loop narrative; splitting hides the lifecycle
pub fn spawn_engine_thread(
    cfg: &crate::config::SuggestionsConfig,
    safra: &crate::safra::SafraConfig,
    janitors: &crate::config::JanitorsConfig,
) {
    let sugg_cfg = cfg.clone();
    let safra_cfg = safra.clone();
    let janitors_cfg = janitors.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = ENGINE_CONTROL.set(EngineControl { tx });
    let res = std::thread::Builder::new()
        .name("mado-suggest".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("suggest-tokio")
                .build()
            {
                Ok(rt) => rt.block_on(async move {
                    let env: Arc<dyn SuggestionEnvironment> = Arc::new(RealEnvironment::discover());
                    let store = store();
                    let path = state_path();
                    let mut current = (sugg_cfg, safra_cfg, janitors_cfg);
                    // The janitor plane rides THIS loop's tick (see
                    // `crate::janitors` for why: the tick already exists,
                    // runs even when suggestions are disabled, and is
                    // hot-reload-aware). Per-janitor cadence is gated
                    // inside the runner; the shared bus is process-global.
                    let mut janitor_runner =
                        crate::janitors::JanitorRunner::from_config(&current.2);
                    let janitor_env = crate::janitors::RealJanitorEnv;
                    // Config-gated (INSIDE the thread): enabled ⇒ live
                    // engine; disabled ⇒ parked loop holding the control
                    // channel so a hot-enable is one Swap away.
                    let (mut engine, mut knobs) = if current.0.enabled {
                        let (e, k) = build_engine(&current.0, &current.1, &env, &store);
                        (Some(e), k)
                    } else {
                        tracing::debug!(
                            "suggestion stream disabled (suggestions.enabled = false) — control plane parked"
                        );
                        (None, LoopKnobs::from_config(&current.0, &engine_config_from(&current.0)))
                    };
                    // Warm restart: re-surface the last-known tasks INSTANTLY
                    // (ages rebased to the snapshot's save time), then age out
                    // anything already stale AT SAVE, before the watchers
                    // re-poll. The picker is populated on the first frame.
                    // Deliberately NOT election-gated: a loser instance loads
                    // too, so a second window boots with the live board and
                    // the persisted dismissal stickiness intact. Loads even
                    // when boot-disabled (cheap; a later hot-enable then
                    // starts from the last-known board, not a blank one).
                    if knobs.persist {
                        let now_ms = env.now_unix().saturating_mul(1000);
                        store.load_file(&path, store::SNAPSHOT_MAGIC, now_ms);
                        izumi::maintain::maintenance_tick(
                            &store,
                            |k| knobs.ttl_map.get(&k).copied().unwrap_or(knobs.global_ttl_ms),
                            knobs.max_entries,
                            now_ms,
                        );
                    }
                    // Maintenance loop — the SINGLE owner of decay + debounced
                    // persist, off the watcher hot path. The watchers only
                    // ever touch RAM; this coalesces a startup burst of first
                    // ticks into ONE disk write, and only when the change-
                    // generation actually advanced. Keeps the runtime + engine
                    // alive for the process lifetime. The select's second arm
                    // is the hot-reload ingress: a Swap tears down the running
                    // engine (stop() aborts every watcher task) and rebuilds
                    // from the NEW config sections — same path for enable,
                    // disable, and reconfigure.
                    let mut last_gen = store.generation();
                    let mut tick = tokio::time::interval(knobs.debounce);
                    loop {
                        tokio::select! {
                            _ = tick.tick() => {
                                let now_ms = env.now_unix().saturating_mul(1000);
                                // The shared izumi maintenance tick: per-source
                                // decay + the hard gc cap, in one call.
                                izumi::maintain::maintenance_tick(
                                    &store,
                                    |k| knobs.ttl_map.get(&k).copied().unwrap_or(knobs.global_ttl_ms),
                                    knobs.max_entries,
                                    now_ms,
                                );
                                let current_gen = store.generation();
                                // The writer election is RE-CHECKED every tick (a
                                // cheap non-blocking flock attempt when not already
                                // held), so a surviving instance picks up the writer
                                // role when the previous winner exits — persistence
                                // never silently dies with the first process.
                                if knobs.persist
                                    && current_gen != last_gen
                                    && crate::single_writer::is_writer()
                                {
                                    store.persist_file(&path, store::SNAPSHOT_MAGIC, now_ms);
                                    last_gen = current_gen;
                                }
                                // Praça persistence rides the same maintenance tick
                                // (internally change-gated + writer-election-gated) —
                                // saved presets survive restarts with zero extra
                                // threads and zero GUI-hot-path writes.
                                crate::praca_store::maintenance_tick();
                                // The janitor plane: per-janitor cadence gated
                                // inside; findings publish on the fiber bus;
                                // remediation only under Authority::Effect.
                                janitor_runner.tick(
                                    &janitor_env,
                                    crate::fibers::bus(),
                                    now_ms,
                                );
                            }
                            cmd = rx.recv() => match cmd {
                                Some(EngineCommand::Swap(pair)) => {
                                    // Idempotence gate: both config sections
                                    // derive PartialEq, so double-fire from
                                    // the two render adapters (and any
                                    // renderer-only config edit) is free.
                                    if *pair == current {
                                        continue;
                                    }
                                    if let Some(e) = engine.take() {
                                        e.stop();
                                    }
                                    let janitors_changed = pair.2 != current.2;
                                    current = *pair;
                                    if janitors_changed {
                                        // Rebuild the janitor plane from the
                                        // new section (fresh grace/streak
                                        // state — deliberate: new knobs, new
                                        // clocks). Left untouched on
                                        // suggestions-only edits so janitor
                                        // observation state survives them.
                                        janitor_runner =
                                            crate::janitors::JanitorRunner::from_config(
                                                &current.2,
                                            );
                                    }
                                    if current.0.enabled {
                                        let (e, k) =
                                            build_engine(&current.0, &current.1, &env, &store);
                                        engine = Some(e);
                                        knobs = k;
                                    } else {
                                        knobs = LoopKnobs::from_config(
                                            &current.0,
                                            &engine_config_from(&current.0),
                                        );
                                    }
                                    tick = tokio::time::interval(knobs.debounce);
                                    tracing::info!(
                                        enabled = current.0.enabled,
                                        "suggestion engine hot-swapped from config edit"
                                    );
                                }
                                // Unreachable while ENGINE_CONTROL holds a
                                // sender for the process lifetime; park
                                // defensively rather than busy-spin on a
                                // closed channel.
                                None => {
                                    tracing::warn!(
                                        "engine control channel closed — hot-reload disabled"
                                    );
                                    tokio::time::sleep(std::time::Duration::from_secs(3600))
                                        .await;
                                }
                            }
                        }
                    }
                }),
                Err(e) => {
                    tracing::warn!(err = %e, "could not create suggestion tokio runtime");
                }
            }
        });
    if let Err(e) = res {
        tracing::warn!(err = %e, "could not spawn mado-suggest thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test (not two) so the process-global env mutations can't race in the
    /// parallel test runner — nothing else reads these vars.
    #[test]
    fn state_path_resolves_overrides_then_state_dir() {
        // SAFETY: env mutation is `unsafe` in edition 2024; this is the sole
        // mutator of these vars and runs its set/clear sequentially.
        unsafe {
            // Explicit DB override wins outright.
            std::env::set_var("MADO_SUGGEST_DB", "/tmp/mado-explicit/snap.json");
            assert_eq!(state_path(), PathBuf::from("/tmp/mado-explicit/snap.json"));

            // Without the explicit override, the state-dir override applies, under
            // a `mado/` subdir.
            std::env::remove_var("MADO_SUGGEST_DB");
            std::env::set_var("MADO_STATE_DIR", "/tmp/mado-state-test");
            assert_eq!(
                state_path(),
                PathBuf::from("/tmp/mado-state-test/mado/suggestions.json")
            );

            std::env::remove_var("MADO_STATE_DIR");
        }
    }

    /// The verdict truth table as mado's borders see it — `status` alone
    /// cannot separate "failing right now" from "never worked" from "never
    /// looked", which is the confusion that let dead lanes read as quiet ones.
    ///
    /// izumi owns the exhaustive `(status × evidence)` table; this asserts the
    /// three rows mado's own gates hang on, and — the load-bearing row — that
    /// **Unknown is not a finding**. A never-polled lane is evidence in
    /// neither direction, so it must not answer `needs_intervention`; the day
    /// it does, every board row and footer in this crate starts firing on
    /// nothing.
    #[test]
    fn verdict_separates_never_worked_from_failing_now_and_from_never_looked() {
        // A lane under observation since t=1_000, polled at t=10_000.
        let polled = |status: SourceStatus, last_ok_ms: u64| SourceHealth {
            status,
            last_poll_ms: 10_000,
            last_ok_ms,
            first_poll_ms: 1_000,
        };
        // A lane whose instrument has never run — no poll, no success.
        let never_polled = |status: SourceStatus| SourceHealth {
            status,
            last_poll_ms: 0,
            last_ok_ms: 0,
            first_poll_ms: 0,
        };
        // Ok is Ok regardless of history.
        assert_eq!(polled(SourceStatus::Ok, 0).verdict(), HealthVerdict::Ok);
        assert_eq!(polled(SourceStatus::Ok, 500).verdict(), HealthVerdict::Ok);
        for status in [
            SourceStatus::Error,
            SourceStatus::AuthMissing,
            SourceStatus::TimedOut,
            SourceStatus::Unconfigured,
        ] {
            // The SAME status splits three ways on the EVIDENCE.
            assert_eq!(
                polled(status, 0).ok_evidence(),
                OkEvidence::NeverSucceeded { since_ms: 1_000 },
                "{status:?} polled with no success is a finding about the source"
            );
            assert_eq!(
                polled(status, 0).verdict(),
                HealthVerdict::Blind,
                "{status:?} polled with no success on record is blind"
            );
            assert_eq!(
                polled(status, 500).verdict(),
                HealthVerdict::Degraded,
                "{status:?} with a success on record is weather"
            );
            assert_eq!(
                never_polled(status).ok_evidence(),
                OkEvidence::Unobserved,
                "{status:?} never polled is NO evidence, not bad evidence"
            );
            assert_eq!(
                never_polled(status).verdict(),
                HealthVerdict::Unknown,
                "{status:?} never polled is unknown, NEVER blind"
            );
        }
        // `first_seen_at` reproduces a source's first recorded poll: the
        // window starts now, and only an Ok sets the latch.
        let fresh_ok = SourceHealth::first_seen_at(SourceStatus::Ok, 7_000);
        assert_eq!(fresh_ok.verdict(), HealthVerdict::Ok);
        assert_eq!(
            fresh_ok.ok_evidence(),
            OkEvidence::Succeeded { at_ms: 7_000 }
        );
        let fresh_bad = SourceHealth::first_seen_at(SourceStatus::Error, 7_000);
        assert_eq!(fresh_bad.verdict(), HealthVerdict::Blind);
        assert_eq!(
            fresh_bad.ok_evidence(),
            OkEvidence::NeverSucceeded { since_ms: 7_000 },
            "the claim rests on the window it can actually point at"
        );
        // Intervention is exactly Blind — the gate the board row hangs on.
        // Unknown sits with Ok and Degraded on the "files nothing" side.
        assert!(HealthVerdict::Blind.needs_intervention());
        assert!(!HealthVerdict::Degraded.needs_intervention());
        assert!(!HealthVerdict::Ok.needs_intervention());
        assert!(
            !HealthVerdict::Unknown.needs_intervention(),
            "gating on 'we have not looked' is gating on nothing"
        );
    }

    /// The agent face carries the verdict too, from the same definition —
    /// so `board_json` and the operator's footer can never disagree about
    /// which lanes have never once succeeded.
    #[test]
    fn board_json_health_carries_the_verdict_beside_ever_ok() {
        let store = store();
        store.record_poll(SourceKind::TendRepos, SourceStatus::Ok, 1_000);
        store.record_poll(SourceKind::GrafanaAlerts, SourceStatus::Error, 2_000);
        let json = board_json(10);
        let health = json["health"].as_array().expect("health block");
        let find = |slug: &str| {
            health
                .iter()
                .find(|h| h["source"] == slug)
                .unwrap_or_else(|| panic!("{slug} missing from health"))
                .clone()
        };
        let ok = find("tend-repos");
        assert_eq!(ok["verdict"], "ok");
        assert_eq!(ok["needs_intervention"], false);
        let blind = find("grafana-alerts");
        // The compact machine token, byte-identical to what mado's retired
        // `HealthVerdict::label()` emitted. izumi's `label()` is the
        // fixed-width board form (`"BLIND   "`), so shipping it here would
        // have silently changed the wire under every agent consumer.
        assert_eq!(blind["verdict"], "blind");
        assert_ne!(blind["verdict"], izumi::HealthVerdict::Blind.label());
        assert_eq!(blind["needs_intervention"], true);
        // `ever_ok` stays on the wire — the verdict is additive, not a
        // replacement, so existing agent consumers keep parsing.
        assert_eq!(blind["ever_ok"], false);
        assert_eq!(ok["ever_ok"], true);
    }

    /// The wire-compat anchors the extraction must never move: the catalog's
    /// serde form is the kebab slug, ids derive byte-identically (fnv1a over
    /// `slug ':' key`), and the snapshot magic is the mado original VERBATIM
    /// (trailing newline included).
    #[test]
    fn izumi_shim_preserves_the_mado_wire_contract() {
        // Catalog shape: 29 variants, declaration order = Ord.
        assert_eq!(SourceKind::ALL.len(), 29);
        assert!(SourceKind::GitBranchPr < SourceKind::Agent);
        // Serde wire form is the kebab slug (byte-identical to the old
        // rename_all = "kebab-case" derive).
        assert_eq!(
            serde_json::to_string(&SourceKind::GithubReviewRequested).unwrap(),
            "\"github-review-requested\""
        );
        assert_eq!(
            SourceKind::from_slug("breathe-conflict"),
            Some(SourceKind::BreatheConflict)
        );
        // Id derivation: fnv1a over `slug ':' key`, unchanged.
        let id = SuggestionId::derive(SourceKind::TendRepos, "mado");
        let mut buf = String::from("tend-repos");
        buf.push(':');
        buf.push_str("mado");
        assert_eq!(id.0, izumi::fnv1a(buf.as_bytes()));
        // The snapshot magic is passed verbatim — trailing newline INCLUDED
        // (dropping it would silently orphan every persisted board + preset).
        assert_eq!(store::SNAPSHOT_MAGIC, b"mado-suggest v1\n");
        assert_eq!(store::SNAPSHOT_MAGIC.last(), Some(&b'\n'));
        // The framed persist pair round-trips under that magic.
        let framed = izumi::persist::frame_snapshot(store::SNAPSHOT_MAGIC, b"{}");
        assert_eq!(
            store::unframe_snapshot(&framed).as_deref(),
            Some(&b"{}"[..])
        );
    }
}
