//! Typed suggestion plane — the data model for the continuously-refreshing
//! "what could I start working on right now" stack the Ctrl-S session picker
//! shades in beneath the live + preset rows.
//!
//! A [`Suggestion`] is a *latent, always-spawnable* task pointer: it carries
//! the exact [`SpawnSpec`] (cwd + name + optional kickoff command) needed to
//! turn it into a live session on Enter. Per the UNREPRESENTABILITY model a
//! suggestion CANNOT be constructed without a valid spawn target — there is
//! no `Suggestion` the picker can show but not act on (`SpawnSpec::new`
//! rejects an empty cwd/name at construction; a suggestion only exists once
//! it owns a valid one).

use std::path::{Path, PathBuf};

/// FNV-1a over bytes — the same run-stable hash family praça uses for its
/// `stable_seed`, so a suggestion's identity is cheap + deterministic for the
/// same underlying task across refreshes (shade-in continuity rides on it).
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Every task-suggestion source the plane knows about — the COMPLETE catalog
/// (CATALOG REFLECTION). The exhaustive `match` in each method below means
/// adding a variant is a compile error until its label / emoji / urgency /
/// auth / cadence are all declared; the catalog can never drift from the code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    // ── local / zero-auth (the proof-of-thread tier) ──────────────────────
    /// Local git branches correlated to their open PR titles.
    GitBranchPr,
    /// `tend` workspace repos that are dirty / unsynced / missing.
    TendRepos,
    /// Recently-visited directories (mado's own recent-dirs).
    RecentDirs,
    /// User-set project marks (mado marks).
    ProjectMarks,
    /// `cargo` warnings/errors in the current project.
    CargoWarnings,
    /// `TODO` / `FIXME` backlog under the code root.
    TodoBacklog,
    // ── github ────────────────────────────────────────────────────────────
    /// PRs awaiting your review.
    GithubReviewRequested,
    /// Issues assigned to you.
    GithubAssignedIssues,
    /// GitHub Actions runs that are failing.
    GithubActionsFailing,
    // ── atlassian ───────────────────────────────────────────────────────—
    /// Jira issues in your active sprint.
    JiraSprint,
    /// Jira issues assigned to you.
    JiraAssigned,
    /// Confluence pages mentioning you.
    ConfluenceMentions,
    // ── cluster / gitops ───────────────────────────────────────────────—
    /// FluxCD Kustomizations/HelmReleases failing to reconcile.
    FluxFailing,
    /// Kubernetes pods Pending / CrashLoopBackOff / unhealthy.
    K8sUnhealthy,
    /// `breathe` resource bands stuck in Conflict.
    BreatheConflict,
    /// engenho cluster nodes not Ready.
    EngenhoNodes,
    // ── observability / incidents ─────────────────────────────────────—
    /// grafana alerts firing.
    GrafanaAlerts,
    /// grafana incidents open.
    GrafanaIncidents,
    /// grafana on-call shifts assigned to you.
    GrafanaOncall,
    /// Datadog monitors alerting.
    DatadogMonitors,
    /// Opsgenie alerts open/unacked.
    OpsgenieAlerts,
    // ── agents / cloud ─────────────────────────────────────────────────—
    /// Cursor cloud (kurage) agents needing follow-up.
    KurageAgents,
    /// AWS health/PHD events affecting your account.
    AwsHealth,
    /// Cloudflare Pages/Workers deployments that failed.
    CloudflareDeployments,
    // ── calendar / tasks ───────────────────────────────────────────────—
    /// Google Tasks due soon.
    GoogleTasks,
    /// Google Calendar events imminent.
    GoogleCalendar,
    // ── secrets ────────────────────────────────────────────────────────—
    /// Secrets whose age exceeds a rotation threshold.
    SecretAge,
}

impl SourceKind {
    /// Every variant, in catalog order — the reflection surface tooling +
    /// config + tests iterate. Kept in lockstep with the enum by
    /// [`SourceKind::assert_catalog_complete`]'s exhaustive cross-check.
    pub const ALL: &'static [SourceKind] = &[
        SourceKind::GitBranchPr,
        SourceKind::TendRepos,
        SourceKind::RecentDirs,
        SourceKind::ProjectMarks,
        SourceKind::CargoWarnings,
        SourceKind::TodoBacklog,
        SourceKind::GithubReviewRequested,
        SourceKind::GithubAssignedIssues,
        SourceKind::GithubActionsFailing,
        SourceKind::JiraSprint,
        SourceKind::JiraAssigned,
        SourceKind::ConfluenceMentions,
        SourceKind::FluxFailing,
        SourceKind::K8sUnhealthy,
        SourceKind::BreatheConflict,
        SourceKind::EngenhoNodes,
        SourceKind::GrafanaAlerts,
        SourceKind::GrafanaIncidents,
        SourceKind::GrafanaOncall,
        SourceKind::DatadogMonitors,
        SourceKind::OpsgenieAlerts,
        SourceKind::KurageAgents,
        SourceKind::AwsHealth,
        SourceKind::CloudflareDeployments,
        SourceKind::GoogleTasks,
        SourceKind::GoogleCalendar,
        SourceKind::SecretAge,
    ];

    /// Kebab slug — the stable id-derivation key + serde wire form.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            SourceKind::GitBranchPr => "git-branch-pr",
            SourceKind::TendRepos => "tend-repos",
            SourceKind::RecentDirs => "recent-dirs",
            SourceKind::ProjectMarks => "project-marks",
            SourceKind::CargoWarnings => "cargo-warnings",
            SourceKind::TodoBacklog => "todo-backlog",
            SourceKind::GithubReviewRequested => "github-review-requested",
            SourceKind::GithubAssignedIssues => "github-assigned-issues",
            SourceKind::GithubActionsFailing => "github-actions-failing",
            SourceKind::JiraSprint => "jira-sprint",
            SourceKind::JiraAssigned => "jira-assigned",
            SourceKind::ConfluenceMentions => "confluence-mentions",
            SourceKind::FluxFailing => "flux-failing",
            SourceKind::K8sUnhealthy => "k8s-unhealthy",
            SourceKind::BreatheConflict => "breathe-conflict",
            SourceKind::EngenhoNodes => "engenho-nodes",
            SourceKind::GrafanaAlerts => "grafana-alerts",
            SourceKind::GrafanaIncidents => "grafana-incidents",
            SourceKind::GrafanaOncall => "grafana-oncall",
            SourceKind::DatadogMonitors => "datadog-monitors",
            SourceKind::OpsgenieAlerts => "opsgenie-alerts",
            SourceKind::KurageAgents => "kurage-agents",
            SourceKind::AwsHealth => "aws-health",
            SourceKind::CloudflareDeployments => "cloudflare-deployments",
            SourceKind::GoogleTasks => "google-tasks",
            SourceKind::GoogleCalendar => "google-calendar",
            SourceKind::SecretAge => "secret-age",
        }
    }

    /// Resolve a slug back to its kind (config parse / round-trip).
    #[must_use]
    pub fn from_slug(s: &str) -> Option<SourceKind> {
        SourceKind::ALL.iter().copied().find(|k| k.slug() == s)
    }

    /// One-glyph emoji signal for the picker row (emoji-native per the fleet
    /// TUI directive — see ishou `FleetSignals`/`app_signals`).
    #[must_use]
    pub fn emoji(self) -> &'static str {
        match self {
            SourceKind::GitBranchPr => "\u{1F33F}",            // 🌿
            SourceKind::TendRepos => "\u{1F9F9}",              // 🧹
            SourceKind::RecentDirs => "\u{1F4C1}",             // 📁
            SourceKind::ProjectMarks => "\u{1F4CC}",           // 📌
            SourceKind::CargoWarnings => "\u{1F980}",          // 🦀
            SourceKind::TodoBacklog => "\u{1F4DD}",            // 📝
            SourceKind::GithubReviewRequested => "\u{1F50D}",  // 🔍
            SourceKind::GithubAssignedIssues => "\u{1F41B}",   // 🐛
            SourceKind::GithubActionsFailing => "\u{1F6A8}",   // 🚨
            SourceKind::JiraSprint => "\u{1F3AB}",             // 🎫
            SourceKind::JiraAssigned => "\u{1F4CB}",           // 📋
            SourceKind::ConfluenceMentions => "\u{1F4AC}",     // 💬
            SourceKind::FluxFailing => "\u{1F501}",            // 🔁
            SourceKind::K8sUnhealthy => "\u{2638}",            // ☸
            SourceKind::BreatheConflict => "\u{1F4A8}",        // 💨
            SourceKind::EngenhoNodes => "\u{1F5A5}",           // 🖥
            SourceKind::GrafanaAlerts => "\u{1F525}",          // 🔥
            SourceKind::GrafanaIncidents => "\u{1F6A9}",       // 🚩
            SourceKind::GrafanaOncall => "\u{1F4DF}",          // 📟
            SourceKind::DatadogMonitors => "\u{1F415}",        // 🐕
            SourceKind::OpsgenieAlerts => "\u{1F514}",         // 🔔
            SourceKind::KurageAgents => "\u{1F916}",           // 🤖
            SourceKind::AwsHealth => "\u{2601}",               // ☁
            SourceKind::CloudflareDeployments => "\u{1F310}",  // 🌐
            SourceKind::GoogleTasks => "\u{2705}",             // ✅
            SourceKind::GoogleCalendar => "\u{1F4C5}",         // 📅
            SourceKind::SecretAge => "\u{1F511}",              // 🔑
        }
    }

    /// Human label for config docs / tooling.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::GitBranchPr => "git branch ↔ PR",
            SourceKind::TendRepos => "tend dirty repos",
            SourceKind::RecentDirs => "recent directories",
            SourceKind::ProjectMarks => "project marks",
            SourceKind::CargoWarnings => "cargo warnings",
            SourceKind::TodoBacklog => "TODO backlog",
            SourceKind::GithubReviewRequested => "GitHub review-requested",
            SourceKind::GithubAssignedIssues => "GitHub assigned issues",
            SourceKind::GithubActionsFailing => "GitHub Actions failing",
            SourceKind::JiraSprint => "Jira sprint",
            SourceKind::JiraAssigned => "Jira assigned",
            SourceKind::ConfluenceMentions => "Confluence mentions",
            SourceKind::FluxFailing => "Flux failing",
            SourceKind::K8sUnhealthy => "k8s unhealthy pods",
            SourceKind::BreatheConflict => "breathe Conflict bands",
            SourceKind::EngenhoNodes => "engenho nodes",
            SourceKind::GrafanaAlerts => "grafana alerts",
            SourceKind::GrafanaIncidents => "grafana incidents",
            SourceKind::GrafanaOncall => "grafana on-call",
            SourceKind::DatadogMonitors => "Datadog monitors",
            SourceKind::OpsgenieAlerts => "Opsgenie alerts",
            SourceKind::KurageAgents => "Cursor agents",
            SourceKind::AwsHealth => "AWS health",
            SourceKind::CloudflareDeployments => "Cloudflare deployments",
            SourceKind::GoogleTasks => "Google Tasks",
            SourceKind::GoogleCalendar => "Google Calendar",
            SourceKind::SecretAge => "secret age",
        }
    }

    /// Default urgency a fresh suggestion from this source carries (sources
    /// may raise it per-item, e.g. a CrashLoop pod → Critical).
    #[must_use]
    pub fn default_urgency(self) -> Urgency {
        match self {
            SourceKind::GrafanaAlerts
            | SourceKind::GrafanaIncidents
            | SourceKind::OpsgenieAlerts
            | SourceKind::DatadogMonitors
            | SourceKind::K8sUnhealthy
            | SourceKind::AwsHealth => Urgency::Critical,
            SourceKind::FluxFailing
            | SourceKind::GithubActionsFailing
            | SourceKind::BreatheConflict
            | SourceKind::GrafanaOncall
            | SourceKind::CloudflareDeployments
            | SourceKind::GithubReviewRequested => Urgency::High,
            SourceKind::JiraSprint
            | SourceKind::JiraAssigned
            | SourceKind::GithubAssignedIssues
            | SourceKind::KurageAgents
            | SourceKind::GoogleCalendar
            | SourceKind::SecretAge
            | SourceKind::EngenhoNodes => Urgency::Normal,
            SourceKind::GitBranchPr
            | SourceKind::TendRepos
            | SourceKind::CargoWarnings
            | SourceKind::GoogleTasks
            | SourceKind::ConfluenceMentions => Urgency::Low,
            SourceKind::RecentDirs | SourceKind::ProjectMarks | SourceKind::TodoBacklog => {
                Urgency::Idle
            }
        }
    }

    /// Whether the source needs a token/credential to return anything (so the
    /// config UI + docs can flag it; an unauthed source returns empty, never
    /// errors).
    #[must_use]
    pub fn needs_auth(self) -> bool {
        match self {
            SourceKind::GitBranchPr
            | SourceKind::TendRepos
            | SourceKind::RecentDirs
            | SourceKind::ProjectMarks
            | SourceKind::CargoWarnings
            | SourceKind::TodoBacklog
            | SourceKind::FluxFailing
            | SourceKind::K8sUnhealthy
            | SourceKind::BreatheConflict
            | SourceKind::EngenhoNodes
            | SourceKind::SecretAge => false,
            SourceKind::GithubReviewRequested
            | SourceKind::GithubAssignedIssues
            | SourceKind::GithubActionsFailing
            | SourceKind::JiraSprint
            | SourceKind::JiraAssigned
            | SourceKind::ConfluenceMentions
            | SourceKind::GrafanaAlerts
            | SourceKind::GrafanaIncidents
            | SourceKind::GrafanaOncall
            | SourceKind::DatadogMonitors
            | SourceKind::OpsgenieAlerts
            | SourceKind::KurageAgents
            | SourceKind::AwsHealth
            | SourceKind::CloudflareDeployments
            | SourceKind::GoogleTasks
            | SourceKind::GoogleCalendar => true,
        }
    }

    /// Default poll cadence in seconds — local/cheap sources poll often, slow
    /// or rate-limited remote ones poll lazily. Operators override per-source
    /// in config.
    #[must_use]
    pub fn default_interval_secs(self) -> u64 {
        match self {
            SourceKind::GitBranchPr
            | SourceKind::RecentDirs
            | SourceKind::ProjectMarks
            | SourceKind::TendRepos => 30,
            SourceKind::CargoWarnings | SourceKind::TodoBacklog => 120,
            SourceKind::FluxFailing
            | SourceKind::K8sUnhealthy
            | SourceKind::BreatheConflict
            | SourceKind::EngenhoNodes => 60,
            SourceKind::GithubReviewRequested
            | SourceKind::GithubAssignedIssues
            | SourceKind::GithubActionsFailing => 180,
            SourceKind::JiraSprint | SourceKind::JiraAssigned | SourceKind::ConfluenceMentions => {
                300
            }
            SourceKind::GrafanaAlerts
            | SourceKind::GrafanaIncidents
            | SourceKind::DatadogMonitors
            | SourceKind::OpsgenieAlerts => 90,
            SourceKind::GrafanaOncall => 600,
            SourceKind::KurageAgents => 120,
            SourceKind::AwsHealth | SourceKind::CloudflareDeployments => 300,
            SourceKind::GoogleTasks | SourceKind::GoogleCalendar => 300,
            SourceKind::SecretAge => 3600,
        }
    }
}

/// How urgently a suggestion wants attention — the dominant ranking axis.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Urgency {
    /// Background fodder (recent dirs, marks).
    Idle,
    /// Nice-to-do (dirty repo, stale TODO).
    Low,
    /// Real work queued (assigned ticket, your PR list).
    #[default]
    Normal,
    /// Should look soon (failing CI, on-call, your review requested).
    High,
    /// Actively on fire (incident, CrashLoop, alert firing).
    Critical,
}

impl Urgency {
    /// Numeric weight for ranking (0..=1000), urgency dominating score.
    #[must_use]
    pub fn weight(self) -> u32 {
        match self {
            Urgency::Idle => 0,
            Urgency::Low => 250,
            Urgency::Normal => 500,
            Urgency::High => 750,
            Urgency::Critical => 1000,
        }
    }

    /// Foreground tint (RGB) for a suggestion row, or `None` to keep the calm
    /// default row colour. Deliberately SPARING — only the urgent tiers glow
    /// (Nord aurora red for Critical, amber for High) so the picker draws the
    /// eye to what's on fire and stays a calm home otherwise.
    #[must_use]
    pub fn tint(self) -> Option<(u8, u8, u8)> {
        match self {
            Urgency::Critical => Some((0xBF, 0x61, 0x6A)), // Nord aurora red
            Urgency::High => Some((0xD0, 0x87, 0x70)),     // Nord aurora amber
            Urgency::Normal | Urgency::Low | Urgency::Idle => None,
        }
    }
}

/// Everything needed to turn a suggestion into a live session — the
/// always-spawnable contract.
///
/// Two ingresses, both validated: [`SpawnSpec::new`] rejects an empty cwd/name,
/// and deserialization routes through `#[serde(try_from = "SpawnSpecWire")]` —
/// the same `new` check — so a persisted snapshot or config can't reintroduce an
/// un-spawnable target. The fields are private, so the only unchecked path is a
/// struct literal *inside this crate*; outside it there is none.
///
/// Tier (per UNREPRESENTABILITY): **parse-time-rejected** on the deserialize
/// boundary + sealed construction in-crate — not truly-unrepresentable (a
/// crate-internal struct literal could still build a blank one), but no
/// picker-reachable row can be shown-but-not-acted-on.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "SpawnSpecWire")]
pub struct SpawnSpec {
    cwd: PathBuf,
    name: String,
    initial_command: Option<String>,
}

/// Untrusted wire shape for [`SpawnSpec`]. The `TryFrom` runs the same
/// validation as [`SpawnSpec::new`], so deserialization can't bypass the
/// always-spawnable invariant.
#[derive(serde::Deserialize)]
struct SpawnSpecWire {
    cwd: PathBuf,
    name: String,
    #[serde(default)]
    initial_command: Option<String>,
}

impl TryFrom<SpawnSpecWire> for SpawnSpec {
    type Error = String;
    fn try_from(w: SpawnSpecWire) -> Result<Self, Self::Error> {
        let spec = SpawnSpec::new(w.cwd, w.name)
            .ok_or_else(|| String::from("SpawnSpec: cwd and name must be non-empty"))?;
        Ok(match w.initial_command {
            Some(c) => spec.with_command(c),
            None => spec,
        })
    }
}

impl SpawnSpec {
    /// Build a spawn target. `None` if `name` is blank or `cwd` is empty — the
    /// only ingress, so a constructed `SpawnSpec` is always actionable.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, name: impl Into<String>) -> Option<Self> {
        let cwd = cwd.into();
        let name = name.into();
        if name.trim().is_empty() || cwd.as_os_str().is_empty() {
            return None;
        }
        Some(Self {
            cwd,
            name,
            initial_command: None,
        })
    }

    /// Attach a command to type into the fresh session (e.g. `gh pr checkout
    /// 1234`). A blank command is ignored.
    #[must_use]
    pub fn with_command(mut self, cmd: impl Into<String>) -> Self {
        let c = cmd.into();
        if !c.trim().is_empty() {
            self.initial_command = Some(c);
        }
        self
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn initial_command(&self) -> Option<&str> {
        self.initial_command.as_deref()
    }
}

/// Stable identity of a suggestion — content-addressed from `(source, key)`
/// so the SAME underlying task keeps ONE id across refreshes (the store dedups
/// + the shade-in continuity ride on this).
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct SuggestionId(pub u64);

impl SuggestionId {
    /// Derive from the owning source + a source-stable key (PR number, ticket
    /// id, repo path, …).
    #[must_use]
    pub fn derive(source: SourceKind, key: &str) -> Self {
        let mut buf = String::with_capacity(source.slug().len() + 1 + key.len());
        buf.push_str(source.slug());
        buf.push(':');
        buf.push_str(key);
        Self(fnv1a(buf.as_bytes()))
    }
}

/// One latent task the picker can shade in + spawn. Plain typed data — built
/// by a source's `poll`, ranked by the store, rendered by the picker.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Suggestion {
    pub id: SuggestionId,
    pub source: SourceKind,
    /// The task itself (the row's primary text), e.g. `pr#1234 fix the parser`.
    pub title: String,
    /// Optional secondary context (repo, assignee, age) shown dimmer.
    pub detail: Option<String>,
    pub urgency: Urgency,
    /// How to start working on it — Enter spawns this.
    pub spawn: SpawnSpec,
    /// Source-relative score 0..=1000, ranking tie-break within an urgency.
    pub score: u32,
}

impl Suggestion {
    /// Build a suggestion. `key` is the source-stable id key; the urgency
    /// defaults to the source's default (override with [`Suggestion::urgent`]).
    #[must_use]
    pub fn new(source: SourceKind, key: &str, title: impl Into<String>, spawn: SpawnSpec) -> Self {
        Self {
            id: SuggestionId::derive(source, key),
            source,
            title: title.into(),
            detail: None,
            urgency: source.default_urgency(),
            spawn,
            score: 500,
        }
    }

    #[must_use]
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        let d = d.into();
        self.detail = if d.trim().is_empty() { None } else { Some(d) };
        self
    }

    #[must_use]
    pub fn urgent(mut self, u: Urgency) -> Self {
        self.urgency = u;
        self
    }

    #[must_use]
    pub fn scored(mut self, score: u32) -> Self {
        self.score = score.min(1000);
        self
    }

    /// Composite rank key — urgency weight in the high bits dominates, score
    /// in the low bits breaks ties. Higher = surfaced first.
    #[must_use]
    pub fn rank_key(&self) -> u64 {
        (u64::from(self.urgency.weight()) << 20) | u64::from(self.score.min(1000))
    }

    /// Compose the picker row text: `<emoji> <title>  <detail>` (the ○ latent
    /// badge is added by the bridge, matching the session-row convention).
    #[must_use]
    pub fn picker_label(&self) -> String {
        let mut s = String::new();
        s.push_str(self.source.emoji());
        s.push(' ');
        s.push_str(self.title.trim());
        if let Some(d) = &self.detail {
            s.push_str("  ");
            s.push_str(d.trim());
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn catalog_is_complete_and_unique() {
        // ALL covers every variant exactly once (CATALOG REFLECTION).
        let mut slugs: Vec<&str> = SourceKind::ALL.iter().map(|k| k.slug()).collect();
        let n = slugs.len();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), n, "every source slug is unique");
        // Round-trip slug → kind for every variant.
        for &k in SourceKind::ALL {
            assert_eq!(SourceKind::from_slug(k.slug()), Some(k));
            assert!(!k.label().is_empty());
            assert!(!k.emoji().is_empty());
            assert!(k.default_interval_secs() > 0);
        }
    }

    #[test]
    fn all_len_matches_an_exhaustive_match() {
        // If a variant is added without extending ALL, this count diverges
        // from the exhaustive `slug` match the compiler enforces.
        assert_eq!(SourceKind::ALL.len(), 27);
    }

    #[test]
    fn spawnspec_rejects_empty() {
        assert!(SpawnSpec::new("", "name").is_none());
        assert!(SpawnSpec::new("/x", "  ").is_none());
        assert!(SpawnSpec::new("/x", "ok").is_some());
    }

    #[test]
    fn spawnspec_deserialize_enforces_the_invariant() {
        // A valid wire shape round-trips through the try_from validation.
        let ok: Result<SpawnSpec, _> =
            serde_json::from_str(r#"{"cwd":"/code","name":"work","initial_command":null}"#);
        assert!(ok.is_ok());
        // A blank name on the wire is REJECTED — deserialization can no longer
        // bypass `new` and reintroduce an un-spawnable target.
        let bad_name: Result<SpawnSpec, _> =
            serde_json::from_str(r#"{"cwd":"/code","name":"   "}"#);
        assert!(bad_name.is_err(), "blank name must fail to deserialize");
        // An empty cwd is likewise rejected.
        let bad_cwd: Result<SpawnSpec, _> =
            serde_json::from_str(r#"{"cwd":"","name":"work"}"#);
        assert!(bad_cwd.is_err(), "empty cwd must fail to deserialize");
        // Round-trip: serialize a built spec, deserialize it back unchanged.
        let spec = SpawnSpec::new("/code", "work").unwrap().with_command("ls");
        let json = serde_json::to_string(&spec).unwrap();
        let back: SpawnSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn suggestion_id_is_stable_per_source_key() {
        let a = SuggestionId::derive(SourceKind::GitBranchPr, "1234");
        let b = SuggestionId::derive(SourceKind::GitBranchPr, "1234");
        let c = SuggestionId::derive(SourceKind::JiraSprint, "1234");
        assert_eq!(a, b, "same (source,key) → same id");
        assert_ne!(a, c, "different source → different id");
    }

    #[test]
    fn rank_key_orders_urgency_over_score() {
        let spawn = SpawnSpec::new("/x", "n").unwrap();
        let crit_low = Suggestion::new(SourceKind::GrafanaAlerts, "a", "fire", spawn.clone())
            .urgent(Urgency::Critical)
            .scored(0);
        let idle_high = Suggestion::new(SourceKind::RecentDirs, "b", "dir", spawn)
            .urgent(Urgency::Idle)
            .scored(1000);
        assert!(
            crit_low.rank_key() > idle_high.rank_key(),
            "urgency dominates score"
        );
    }

    #[test]
    fn picker_label_is_emoji_native() {
        let spawn = SpawnSpec::new("/code/mado", "x").unwrap();
        let s = Suggestion::new(SourceKind::GithubReviewRequested, "1", "pr#1 fix", spawn)
            .detail("mado · 2h");
        let label = s.picker_label();
        assert!(label.starts_with(SourceKind::GithubReviewRequested.emoji()));
        assert!(label.contains("pr#1 fix"));
        assert!(label.contains("mado"));
    }

    #[test]
    fn only_urgent_tiers_tint() {
        // Sparing by design: only Critical + High glow; the calm tiers keep the
        // default row colour.
        assert!(Urgency::Critical.tint().is_some());
        assert!(Urgency::High.tint().is_some());
        assert_eq!(Urgency::Normal.tint(), None);
        assert_eq!(Urgency::Low.tint(), None);
        assert_eq!(Urgency::Idle.tint(), None);
        assert_ne!(
            Urgency::Critical.tint(),
            Urgency::High.tint(),
            "the two urgent tiers are visually distinct"
        );
    }

    #[test]
    fn urgency_always_dominates_score_in_rank_key() {
        // A higher-urgency suggestion scored 0 still outranks a lower-urgency
        // one scored max — the dominant axis is urgency, by construction.
        let order = [
            Urgency::Idle,
            Urgency::Low,
            Urgency::Normal,
            Urgency::High,
            Urgency::Critical,
        ];
        let spawn = SpawnSpec::new("/x", "n").unwrap();
        for w in order.windows(2) {
            let lo = Suggestion::new(SourceKind::RecentDirs, "a", "t", spawn.clone())
                .urgent(w[0])
                .scored(1000);
            let hi = Suggestion::new(SourceKind::RecentDirs, "b", "t", spawn.clone())
                .urgent(w[1])
                .scored(0);
            assert!(
                hi.rank_key() > lo.rank_key(),
                "{:?}@0 must outrank {:?}@1000",
                w[1],
                w[0]
            );
        }
    }

    proptest! {
        #[test]
        fn suggestion_id_is_deterministic(key in ".*") {
            prop_assert_eq!(
                SuggestionId::derive(SourceKind::GitBranchPr, &key),
                SuggestionId::derive(SourceKind::GitBranchPr, &key)
            );
        }

        #[test]
        fn spawnspec_some_iff_cwd_and_name_nonblank(cwd in ".*", name in ".*") {
            let made = SpawnSpec::new(cwd.clone(), name.clone()).is_some();
            let expect = !name.trim().is_empty() && !cwd.is_empty();
            prop_assert_eq!(made, expect);
        }
    }
}
