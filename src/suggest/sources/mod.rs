//! Provider impls — one [`SuggestionSource`](super::source::SuggestionSource)
//! per [`SourceKind`](super::core::SourceKind).
//!
//! [`registry`] returns every implemented source; the engine consults the
//! shikumi config to decide which to actually run. Each provider is a pure
//! `poll(env)` tested through a `MockEnvironment` — the live wiring (real
//! `Command`/HTTP/secret) is the only un-mocked seam.

use std::path::PathBuf;
use std::sync::Arc;

use super::env::SuggestionEnvironment;
use super::source::SuggestionSource;

pub mod aws_health;
pub mod breathe_conflict;
pub mod cargo_warnings;
pub mod cloudflare_deployments;
pub mod confluence_mentions;
pub mod datadog_monitors;
pub mod engenho_nodes;
pub mod flux_failing;
pub mod git_branch_pr;
pub mod github_actions_failing;
pub mod github_assigned_issues;
pub mod github_review_requested;
pub mod google_calendar;
pub mod google_tasks;
pub mod grafana_alerts;
pub mod grafana_incidents;
pub mod grafana_oncall;
pub mod jira_assigned;
pub mod jira_sprint;
pub mod k8s_unhealthy;
pub mod kurage_agents;
pub mod opsgenie_alerts;
pub mod project_marks;
pub mod recent_dirs;
pub mod secret_age;
pub mod tend_repos;
pub mod todo_backlog;
pub mod util;

/// Every implemented source, in catalog order. The engine runs the
/// config-enabled subset.
#[must_use]
pub fn registry() -> Vec<Arc<dyn SuggestionSource>> {
    vec![
        Arc::new(git_branch_pr::GitBranchPrSource),
        Arc::new(github_review_requested::GithubReviewRequestedSource),
        Arc::new(github_assigned_issues::GithubAssignedIssuesSource),
        Arc::new(github_actions_failing::GithubActionsFailingSource),
        Arc::new(tend_repos::TendReposSource),
        Arc::new(recent_dirs::RecentDirsSource),
        Arc::new(project_marks::ProjectMarksSource),
        Arc::new(cargo_warnings::CargoWarningsSource),
        Arc::new(todo_backlog::TodoBacklogSource),
        Arc::new(jira_sprint::JiraSprintSource),
        Arc::new(jira_assigned::JiraAssignedSource),
        Arc::new(confluence_mentions::ConfluenceMentionsSource),
        Arc::new(flux_failing::FluxFailingSource),
        Arc::new(k8s_unhealthy::K8sUnhealthySource),
        Arc::new(breathe_conflict::BreatheConflictSource),
        Arc::new(engenho_nodes::EngenhoNodesSource),
        Arc::new(grafana_alerts::GrafanaAlertsSource),
        Arc::new(grafana_incidents::GrafanaIncidentsSource),
        Arc::new(grafana_oncall::GrafanaOncallSource),
        Arc::new(datadog_monitors::DatadogMonitorsSource),
        Arc::new(opsgenie_alerts::OpsgenieAlertsSource),
        Arc::new(kurage_agents::KurageAgentsSource),
        Arc::new(aws_health::AwsHealthSource),
        Arc::new(cloudflare_deployments::CloudflareDeploymentsSource),
        Arc::new(google_tasks::GoogleTasksSource),
        Arc::new(google_calendar::GoogleCalendarSource),
        Arc::new(secret_age::SecretAgeSource),
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

impl SuggestionSource for AgentInjectedSource {
    fn kind(&self) -> super::core::SourceKind {
        super::core::SourceKind::Agent
    }
    fn poll(
        &self,
        _env: &dyn SuggestionEnvironment,
        _cfg: &super::source::SourceConfig,
    ) -> super::source::PollOutcome {
        super::source::PollOutcome::unconfigured()
    }
}

/// Resolve the local working directory for a `owner/name` repo under the
/// operator's code root, following the workspace convention
/// `~/code/${service}/${org}/${repo}` (service defaults to `github`). Falls
/// back to the code root if the conventional path does not exist, so a spawn
/// always has a real cwd.
#[must_use]
pub fn repo_cwd(env: &dyn SuggestionEnvironment, name_with_owner: &str) -> PathBuf {
    let root = env.code_root();
    let mut parts = name_with_owner.splitn(2, '/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or(owner);
    if owner.is_empty() || name.is_empty() {
        return root;
    }
    let candidate = root.join("github").join(owner).join(name);
    if env.path_exists(&candidate) {
        candidate
    } else {
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::core::SourceKind;

    #[test]
    fn registry_kinds_are_unique_and_in_catalog() {
        let reg = registry();
        let mut kinds: Vec<SourceKind> = reg.iter().map(|s| s.kind()).collect();
        let n = kinds.len();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), n, "no source kind registered twice");
        for k in &kinds {
            assert!(SourceKind::ALL.contains(k));
        }
    }

    /// The verification matrix forcing-function (CLOSED-LOOP MASS-SYNTHESIS
    /// rule 1): EVERY catalog source has a registered provider. Add a new
    /// `SourceKind` and forget to register/implement its provider → this fails,
    /// so the catalog can never claim a source the engine can't actually run.
    #[test]
    fn every_catalog_source_is_registered() {
        let registered: std::collections::BTreeSet<SourceKind> =
            registry().iter().map(|s| s.kind()).collect();
        let missing: Vec<&str> = SourceKind::ALL
            .iter()
            .filter(|k| !registered.contains(k))
            .map(|k| k.slug())
            .collect();
        assert!(
            missing.is_empty(),
            "{} catalog source(s) have no registered provider: {}",
            missing.len(),
            missing.join(", ")
        );
        assert_eq!(
            registered.len(),
            SourceKind::ALL.len(),
            "registry covers the whole catalog exactly"
        );
    }

    #[test]
    fn repo_cwd_follows_workspace_convention_when_present() {
        let env = super::super::env::MockEnvironment::new()
            .roots("/code", "/home/op")
            .path("/code/github/pleme-io/mado");
        assert_eq!(
            repo_cwd(&env, "pleme-io/mado"),
            PathBuf::from("/code/github/pleme-io/mado")
        );
        // Missing dir → fall back to the code root (spawn still has a cwd).
        assert_eq!(repo_cwd(&env, "pleme-io/ghost"), PathBuf::from("/code"));
    }
}
