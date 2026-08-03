//! Provider registry — one `izumi::Source` per [`SourceKind`](super::core::SourceKind).
//!
//! The 25 GENERIC providers (github/jira/grafana/k8s/flux/tend/cargo/…) live
//! in the `izumi-sources` crate, each parameterized by the kind it reports
//! (`Provider::new(SourceKind::X)`). What stays mado-local: the two
//! mado-state providers ([`recent_dirs`], [`project_marks`] — they read
//! mado's own files), the push-only [`AgentInjectedSource`] lane, and the
//! safra placeholder. [`registry`] returns every implemented source; the
//! engine consults the shikumi config to decide which to actually run.
//!
//! Shared provider primitives (priority scales, percent-encoding, JSON
//! helpers, the workspace-convention cwd resolver) live in
//! `izumi_sources::util`, re-exported here as [`util`] so the historical
//! `crate::suggest::sources::util::*` paths keep working.

use std::sync::Arc;

use super::core::SourceKind;
use super::env::SuggestionEnvironment;
use super::source::DynSuggestionSource;

pub mod project_marks;
pub mod recent_dirs;

/// Shared provider primitives — re-exported from `izumi_sources::util`
/// (moved there with the provider extraction; same API).
pub mod util {
    #[allow(unused_imports)]
    pub use izumi_sources::util::*;
}

/// Every implemented source, in catalog order. The engine runs the
/// config-enabled subset.
#[must_use]
pub fn registry() -> Vec<Arc<DynSuggestionSource>> {
    vec![
        Arc::new(izumi_sources::GitBranchPr::new(SourceKind::GitBranchPr)),
        Arc::new(izumi_sources::GithubReviewRequested::new(
            SourceKind::GithubReviewRequested,
        )),
        Arc::new(izumi_sources::GithubAssignedIssues::new(
            SourceKind::GithubAssignedIssues,
        )),
        Arc::new(izumi_sources::GithubActionsFailing::new(
            SourceKind::GithubActionsFailing,
        )),
        Arc::new(izumi_sources::TendRepos::new(SourceKind::TendRepos)),
        Arc::new(recent_dirs::RecentDirsSource),
        Arc::new(project_marks::ProjectMarksSource),
        Arc::new(izumi_sources::CargoWarnings::new(SourceKind::CargoWarnings)),
        Arc::new(izumi_sources::TodoBacklog::new(SourceKind::TodoBacklog)),
        Arc::new(izumi_sources::JiraSprint::new(SourceKind::JiraSprint)),
        Arc::new(izumi_sources::JiraAssigned::new(SourceKind::JiraAssigned)),
        Arc::new(izumi_sources::ConfluenceMentions::new(
            SourceKind::ConfluenceMentions,
        )),
        Arc::new(izumi_sources::FluxFailing::new(SourceKind::FluxFailing)),
        Arc::new(izumi_sources::K8sUnhealthy::new(SourceKind::K8sUnhealthy)),
        Arc::new(izumi_sources::BreatheConflict::new(
            SourceKind::BreatheConflict,
        )),
        Arc::new(izumi_sources::EngenhoNodes::new(SourceKind::EngenhoNodes)),
        Arc::new(izumi_sources::GrafanaAlerts::new(SourceKind::GrafanaAlerts)),
        Arc::new(izumi_sources::GrafanaIncidents::new(
            SourceKind::GrafanaIncidents,
        )),
        Arc::new(izumi_sources::GrafanaOncall::new(SourceKind::GrafanaOncall)),
        Arc::new(izumi_sources::DatadogMonitors::new(
            SourceKind::DatadogMonitors,
        )),
        Arc::new(izumi_sources::OpsgenieAlerts::new(
            SourceKind::OpsgenieAlerts,
        )),
        Arc::new(izumi_sources::KurageAgents::new(SourceKind::KurageAgents)),
        Arc::new(izumi_sources::AwsHealth::new(SourceKind::AwsHealth)),
        Arc::new(izumi_sources::CloudflareDeployments::new(
            SourceKind::CloudflareDeployments,
        )),
        Arc::new(izumi_sources::GoogleTasks::new(SourceKind::GoogleTasks)),
        Arc::new(izumi_sources::GoogleCalendar::new(
            SourceKind::GoogleCalendar,
        )),
        Arc::new(izumi_sources::SecretAge::new(SourceKind::SecretAge)),
        // Placeholder adapter (no cells → typed "needs config" health). The
        // engine bootstrap swaps in the config-built adapter when the
        // `safra:` section declares cells — see `suggest::spawn_engine_thread`.
        Arc::new(crate::safra::SafraSuggestionSource::unconfigured()),
        Arc::new(AgentInjectedSource),
    ]
}

/// The push-only agent lane: `suggest_inject` (MCP) upserts rows directly into
/// the store; there is nothing to poll, so the kind is never armed (no watcher
/// runs) and this placeholder exists only so the catalog ↔ registry
/// completeness gate holds. If it were ever polled it reports Unconfigured —
/// honest ("this lane is fed, not fetched"), and crucially never `Fetched` (an
/// observed-empty ingest would wipe the injected rows).
struct AgentInjectedSource;

impl izumi::Source<SourceKind, izumi::SpawnSpec> for AgentInjectedSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Agent
    }
    fn poll(
        &self,
        _env: &dyn SuggestionEnvironment,
        _cfg: &super::source::SourceConfig,
    ) -> super::source::PollOutcome {
        super::source::PollOutcome::unconfigured()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verification matrix forcing-function (CLOSED-LOOP MASS-SYNTHESIS
    /// rule 1), now the shared `izumi_sources::assert_registry_wiring`
    /// invariant: EVERY catalog kind has exactly one registered provider —
    /// no duplicates, no missing, no extras. Add a new `SourceKind` and
    /// forget to register/implement its provider → this fails, so the
    /// catalog can never claim a source the engine can't actually run.
    #[test]
    fn registry_covers_the_catalog_exactly() {
        izumi_sources::assert_registry_wiring(&registry(), SourceKind::ALL);
    }
}
