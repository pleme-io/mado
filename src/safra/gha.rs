//! GitHub Actions deployment-flow source — the third board lane: live
//! deployment runs that change an environment ("a deploy to prod is
//! running/failed — engage it"). Ingests `GET /repos/{owner}/{repo}/actions/runs`
//! per allowlisted repo, filters to env-changing deploy workflows, and
//! normalizes each run to a [`Signal`] tagged with its target environment.
//!
//! **Generic + borealis-clean:** this source names no consumer. The concrete
//! repo allowlist + workflow/env patterns are supplied as a [`GhaFilter`] from
//! config (the private deployment layer carries the site-specific filter); the
//! code + tests here use only generic shapes. See `docs/SAFRA.md` §2.5.
//!
//! **Env denotation:** the critical signal is the target environment, encoded in
//! the run *name* (e.g. "Deploy Service: api, tenant: acme, environment:
//! production, version: 1.2.3" → `acme_production`), since the job-level
//! `environment:` keyword is not exposed on the runs API. [`extract_env`] parses
//! it without a regex dependency.

use super::curated::Severity;
use super::schema::ServiceKind;
use super::signal::Signal;
use super::source::{CellSource, ObserveCtx};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};

/// A typed text matcher — the regex-free way to express workflow-name / env
/// patterns from config. Case-sensitive by construction (GHA names are stable).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextMatch {
    /// The whole string equals this.
    Exact(String),
    /// The string starts with this.
    Prefix(String),
    /// The string ends with this (e.g. `_production`).
    Suffix(String),
    /// The string contains this.
    Contains(String),
}

impl TextMatch {
    #[must_use]
    pub fn matches(&self, s: &str) -> bool {
        match self {
            TextMatch::Exact(p) => s == p,
            TextMatch::Prefix(p) => s.starts_with(p),
            TextMatch::Suffix(p) => s.ends_with(p),
            TextMatch::Contains(p) => s.contains(p),
        }
    }
}

/// True if `s` matches any matcher in `set` (an empty set matches nothing —
/// callers treat "empty allow" as "no constraint" themselves).
fn any_match(set: &[TextMatch], s: &str) -> bool {
    set.iter().any(|m| m.matches(s))
}

/// The config that selects "live deployments to critical environments" — all
/// supplied from config, never hardcoded (borealis hygiene).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GhaFilter {
    /// `owner/repo` allowlist to poll. Empty = poll nothing (off).
    pub repos: Vec<String>,
    /// Trigger events that actually ship to an env (e.g. workflow_dispatch,
    /// deployment, schedule). Empty = no event constraint.
    pub events: Vec<String>,
    /// Branches that deploy (e.g. master, main). Empty = no branch constraint.
    pub branches: Vec<String>,
    /// Workflow-name allow patterns (the deploy/promote/failover flows). Empty =
    /// no allow constraint (every non-denied workflow passes).
    pub workflow_allow: Vec<TextMatch>,
    /// Workflow-name deny patterns (build/scan/test false positives). Always wins.
    pub workflow_deny: Vec<TextMatch>,
    /// Target-environment allow patterns (e.g. `Suffix("_production")`,
    /// `Suffix("_staging")`). Empty = surface every env that parses.
    pub env_allow: Vec<TextMatch>,
    /// Surface successful deploys (as low-severity "fresh" info). When false,
    /// only in-progress + failed deploys reach the board.
    pub surface_success: bool,
}

/// One GitHub Actions run, deserialized from the `workflow_runs` array. Only the
/// fields the source needs; unknown fields are ignored (the API is large).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GhaRun {
    pub id: u64,
    /// The run name (`run-name:`) — carries tenant/env/service/version.
    #[serde(default)]
    pub name: String,
    /// `queued` | `in_progress` | `completed`.
    #[serde(default)]
    pub status: String,
    /// `success` | `failure` | `cancelled` | `timed_out` | null.
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub head_branch: Option<String>,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub actor: Option<GhaActor>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GhaActor {
    #[serde(default)]
    pub login: String,
}

/// The `/actions/runs` response envelope.
#[derive(Debug, Clone, serde::Deserialize)]
struct GhaRunsResponse {
    #[serde(default)]
    workflow_runs: Vec<GhaRun>,
}

/// Parse the `<tenant>_<environment>` target from a run name by scanning for the
/// `tenant:` and `environment:` tokens the deploy pipelines encode. Returns
/// `None` if either token is absent. No regex — a forward token scan.
#[must_use]
pub fn extract_env(run_name: &str) -> Option<String> {
    let tenant = token_after(run_name, "tenant:")?;
    let env = token_after(run_name, "environment:")?;
    let mut s = String::with_capacity(tenant.len() + 1 + env.len());
    s.push_str(tenant);
    s.push('_');
    s.push_str(env);
    Some(s)
}

/// The first whitespace/`,`-delimited word following `marker` in `s`.
fn token_after<'a>(s: &'a str, marker: &str) -> Option<&'a str> {
    let rest = s.split_once(marker)?.1.trim_start();
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ',')
        .unwrap_or(rest.len());
    let tok = &rest[..end];
    if tok.is_empty() { None } else { Some(tok) }
}

/// Severity from a run's status/conclusion: a failed/cancelled/timed-out deploy
/// is Critical (top of the board); a live (in_progress/queued) deploy is a
/// Warning; a successful deploy is Info ("fresh, FYI").
fn severity_of(run: &GhaRun) -> Severity {
    if let Some(c) = run.conclusion.as_deref() {
        return match c {
            "failure" | "cancelled" | "timed_out" | "startup_failure" | "action_required" => {
                Severity::Critical
            }
            "success" => Severity::Info,
            _ => Severity::Warning,
        };
    }
    match run.status.as_str() {
        "in_progress" | "queued" | "requested" | "waiting" | "pending" => Severity::Warning,
        _ => Severity::Info,
    }
}

/// Apply the filter + normalize one run to a [`Signal`], or `None` if the run is
/// not a surface-worthy deployment to an allowed environment. `repo` is the
/// `owner/repo` it came from (the runs endpoint is repo-scoped).
#[must_use]
pub fn run_to_signal(repo: &str, run: &GhaRun, kind: &str, filter: &GhaFilter) -> Option<Signal> {
    // Trigger event must ship to an env.
    if !filter.events.is_empty() && !filter.events.iter().any(|e| e == &run.event) {
        return None;
    }
    // Branch must be a deploying branch.
    if !filter.branches.is_empty() {
        let br = run.head_branch.as_deref().unwrap_or("");
        if !filter.branches.iter().any(|b| b == br) {
            return None;
        }
    }
    // Workflow-name allow (if constrained) AND never a denied one.
    if any_match(&filter.workflow_deny, &run.name) {
        return None;
    }
    if !filter.workflow_allow.is_empty() && !any_match(&filter.workflow_allow, &run.name) {
        return None;
    }
    // The target environment — the critical filter.
    let env = extract_env(&run.name)?;
    if !filter.env_allow.is_empty() && !any_match(&filter.env_allow, &env) {
        return None;
    }
    let severity = severity_of(run);
    if severity == Severity::Info && !filter.surface_success {
        return None;
    }
    // Stable identity: `<repo>#<run-id>` — immutable across refreshes.
    let mut identity = String::with_capacity(repo.len() + 1 + 20);
    identity.push_str(repo);
    identity.push('#');
    identity.push_str(&run.id.to_string());

    let mut sig = Signal::new(env, kind, identity, severity, run.name.clone());
    if let Some(actor) = run.actor.as_ref() {
        if !actor.login.is_empty() {
            let mut d = String::from("by ");
            d.push_str(&actor.login);
            d.push_str(" · ");
            d.push_str(&run.event);
            sig = sig.with_detail(d);
        }
    }
    if !run.html_url.is_empty() {
        sig = sig.with_link(run.html_url.clone());
    }
    Some(sig)
}

/// The GitHub Actions deployment-flow `CellSource`. Holds its [`GhaFilter`]
/// (supplied from config); `observe` polls each allowlisted repo's runs through
/// the mockable environment and normalizes them.
pub struct GhaDeploymentSource {
    filter: GhaFilter,
}

impl GhaDeploymentSource {
    #[must_use]
    pub fn new(filter: GhaFilter) -> Self {
        Self { filter }
    }

    /// The runs URL for one repo against the endpoint's API base. Deterministic
    /// (no volatile query) so it is cacheable + mockable.
    fn runs_url(base: &str, repo: &str) -> String {
        let mut u = String::from(base.trim_end_matches('/'));
        u.push_str("/repos/");
        u.push_str(repo);
        u.push_str("/actions/runs?per_page=30");
        u
    }
}

impl CellSource for GhaDeploymentSource {
    fn service(&self) -> ServiceKind {
        ServiceKind::GitHubActions
    }

    fn observe(&self, env: &dyn SuggestionEnvironment, ctx: &ObserveCtx) -> Vec<Signal> {
        let base = &ctx.endpoint.base_url;
        let kind = &ctx.kind.name;
        let mut out = Vec::new();
        for repo in &self.filter.repos {
            let url = Self::runs_url(base, repo);
            let mut req = HttpReq::new(url);
            if let Some(tok) = ctx.secret.as_deref() {
                req = req.bearer(tok);
            }
            let Some(body) = env.http_get(&req) else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<GhaRunsResponse>(&body) else {
                continue;
            };
            for run in &parsed.workflow_runs {
                if let Some(sig) = run_to_signal(repo, run, kind, &self.filter) {
                    out.push(sig);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::schema::{Endpoint, TrackedDataKind, TrackedEnvironment};
    use super::*;
    use crate::suggest::env::MockEnvironment;

    fn run(
        name: &str,
        status: &str,
        conclusion: Option<&str>,
        event: &str,
        branch: &str,
    ) -> GhaRun {
        GhaRun {
            id: 42,
            name: name.into(),
            status: status.into(),
            conclusion: conclusion.map(Into::into),
            head_branch: Some(branch.into()),
            event: event.into(),
            html_url: "https://github.com/o/r/actions/runs/42".into(),
            actor: Some(GhaActor {
                login: "alice".into(),
            }),
        }
    }

    fn prod_filter() -> GhaFilter {
        GhaFilter {
            repos: vec!["o/r".into()],
            events: vec!["workflow_dispatch".into()],
            branches: vec!["master".into()],
            workflow_allow: vec![TextMatch::Prefix("Deploy".into())],
            workflow_deny: vec![TextMatch::Contains("Build".into())],
            env_allow: vec![
                TextMatch::Suffix("_production".into()),
                TextMatch::Suffix("_staging".into()),
            ],
            surface_success: false,
        }
    }

    #[test]
    fn text_match_variants() {
        assert!(TextMatch::Exact("x".into()).matches("x"));
        assert!(TextMatch::Prefix("De".into()).matches("Deploy"));
        assert!(TextMatch::Suffix("_production".into()).matches("acme_production"));
        assert!(TextMatch::Contains("Build".into()).matches("Build-And-Push"));
        assert!(!TextMatch::Suffix("_production".into()).matches("acme_staging"));
    }

    #[test]
    fn extract_env_parses_tenant_and_environment() {
        let n = "Deploy Service: api, tenant: acme, environment: production, version: 1.2.3";
        assert_eq!(extract_env(n).as_deref(), Some("acme_production"));
        assert_eq!(extract_env("no tokens here"), None);
        assert_eq!(
            extract_env("tenant: globex environment: staging").as_deref(),
            Some("globex_staging")
        );
    }

    #[test]
    fn failed_prod_deploy_is_critical() {
        let r = run(
            "Deploy tenant: acme, environment: production",
            "completed",
            Some("failure"),
            "workflow_dispatch",
            "master",
        );
        let s = run_to_signal("o/r", &r, "deploy-flows", &prod_filter()).expect("surfaced");
        assert_eq!(s.severity, Severity::Critical);
        assert_eq!(s.env, "acme_production");
        assert_eq!(s.identity, "o/r#42");
        assert_eq!(
            s.link.as_deref(),
            Some("https://github.com/o/r/actions/runs/42")
        );
    }

    #[test]
    fn in_progress_deploy_is_warning() {
        let r = run(
            "Deploy tenant: globex, environment: staging",
            "in_progress",
            None,
            "workflow_dispatch",
            "master",
        );
        let s = run_to_signal("o/r", &r, "deploy-flows", &prod_filter()).expect("surfaced");
        assert_eq!(s.severity, Severity::Warning);
        assert_eq!(s.env, "globex_staging");
    }

    #[test]
    fn denied_workflow_and_wrong_branch_and_dev_env_are_dropped() {
        let f = prod_filter();
        // denied (Build)
        assert!(
            run_to_signal(
                "o/r",
                &run(
                    "Deploy Build tenant: acme, environment: production",
                    "completed",
                    Some("failure"),
                    "workflow_dispatch",
                    "master"
                ),
                "k",
                &f
            )
            .is_none()
        );
        // wrong branch
        assert!(
            run_to_signal(
                "o/r",
                &run(
                    "Deploy tenant: acme, environment: production",
                    "in_progress",
                    None,
                    "workflow_dispatch",
                    "feature"
                ),
                "k",
                &f
            )
            .is_none()
        );
        // dev env not in env_allow (prod/staging only)
        assert!(
            run_to_signal(
                "o/r",
                &run(
                    "Deploy tenant: acme, environment: development",
                    "in_progress",
                    None,
                    "workflow_dispatch",
                    "master"
                ),
                "k",
                &f
            )
            .is_none()
        );
        // success dropped when surface_success=false
        assert!(
            run_to_signal(
                "o/r",
                &run(
                    "Deploy tenant: acme, environment: production",
                    "completed",
                    Some("success"),
                    "workflow_dispatch",
                    "master"
                ),
                "k",
                &f
            )
            .is_none()
        );
    }

    #[test]
    fn observe_polls_repo_and_normalizes() {
        let body = r#"{"total_count":2,"workflow_runs":[
            {"id":1,"name":"Deploy tenant: acme, environment: production","status":"completed","conclusion":"failure","head_branch":"master","event":"workflow_dispatch","html_url":"https://x/1","actor":{"login":"bob"}},
            {"id":2,"name":"Build-And-Push tenant: acme, environment: production","status":"in_progress","head_branch":"master","event":"workflow_dispatch","html_url":"https://x/2"}
        ]}"#;
        let url = GhaDeploymentSource::runs_url("https://api.github.com", "o/r");
        let env = MockEnvironment::new()
            .http(url, body)
            .secret_val("github/x/token", "tok");
        let src = GhaDeploymentSource::new(prod_filter());
        let tracked_env = TrackedEnvironment {
            name: "example-gha".into(),
            groups: vec![],
            endpoints: vec![Endpoint {
                service: ServiceKind::GitHubActions,
                base_url: "https://api.github.com".into(),
                secret_ref: Some("github/x/token".into()),
            }],
        };
        let kind = TrackedDataKind {
            name: "deploy-flows".into(),
            service: ServiceKind::GitHubActions,
            groups: vec![],
            ttl_secs: 600,
        };
        let endpoint = &tracked_env.endpoints[0];
        let ctx = ObserveCtx {
            env: &tracked_env,
            kind: &kind,
            endpoint,
            secret: Some("tok".into()),
        };
        let sigs = src.observe(&env, &ctx);
        // run 1 surfaces (failed prod deploy); run 2 is denied (Build).
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].severity, Severity::Critical);
        assert_eq!(sigs[0].identity, "o/r#1");
        assert_eq!(sigs[0].kind, "deploy-flows");
    }
}
