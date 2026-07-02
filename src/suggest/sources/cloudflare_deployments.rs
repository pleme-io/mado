//! `cloudflare-deployments` — failed Cloudflare Pages deployments for a project,
//! surfaced as "go fix the build" suggestions. HTTP against the Cloudflare API;
//! auth is the `cloudflare/api-token` secret (Bearer, overridable via the
//! `secret` param) + `account_id` / `pages_project` params. Enter spawns a
//! session rooted at your code root.
//!
//! Live wiring: `GET https://api.cloudflare.com/client/v4/accounts/<account_id>/
//! pages/projects/<pages_project>/deployments?per_page=N` with `Authorization:
//! Bearer <token>`. Honesty contract: a missing `account_id`/`pages_project` is
//! `Unavailable(Unconfigured)`, a missing token `Unavailable(AuthMissing)`, a
//! failed fetch `Unavailable(Error)` — only an OBSERVED response is `Fetched`
//! (so an API blip never reads as "every deploy is green"). Only deployments
//! whose `latest_stage.status == "failure"` surface.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{HttpReq, SuggestionEnvironment};
use crate::suggest::source::{PollOutcome, SourceConfig, SuggestionSource};

pub struct CloudflareDeploymentsSource;

impl SuggestionSource for CloudflareDeploymentsSource {
    fn kind(&self) -> SourceKind {
        SourceKind::CloudflareDeployments
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> PollOutcome {
        let Some(account) = cfg.param("account_id") else {
            return PollOutcome::unconfigured();
        };
        let Some(project) = cfg.param("pages_project") else {
            return PollOutcome::unconfigured();
        };
        let secret_key = cfg.param("secret").unwrap_or("cloudflare/api-token");
        let Some(token) = env.secret(secret_key) else {
            return PollOutcome::auth_missing();
        };
        let max = cfg.max_items.max(1);
        let mut url = String::from("https://api.cloudflare.com/client/v4/accounts/");
        url.push_str(account);
        url.push_str("/pages/projects/");
        url.push_str(project);
        url.push_str("/deployments?per_page=");
        url.push_str(&max.to_string());
        let req = HttpReq::new(url).bearer(&token);
        let Some(out) = env.http_get(&req) else {
            return PollOutcome::error();
        };
        PollOutcome::Fetched(parse(&out, project, env, max))
    }
}

/// Parse a Cloudflare Pages `…/deployments` response into one suggestion per
/// failed deployment. Pure — the unit the source is tested through.
fn parse(json: &str, project: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    let Ok(resp) = serde_json::from_str::<DeploymentsResponse>(json) else {
        return Vec::new();
    };
    resp.result
        .into_iter()
        .filter(|d| d.latest_stage.status == "failure")
        .take(max)
        .filter_map(|d| {
            let cwd = env.code_root();
            let name = String::from("\u{1F310} deploy"); // 🌐
            let spawn = SpawnSpec::new(cwd, name)?;
            let mut title = String::new();
            title.push_str(project);
            title.push_str(" deploy failed");
            let id = d.id;
            Some(
                Suggestion::new(SourceKind::CloudflareDeployments, &id, title, spawn)
                    .detail(id)
                    .urgent(Urgency::High),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct DeploymentsResponse {
    #[serde(default)]
    result: Vec<DeploymentRow>,
}

#[derive(serde::Deserialize, Default)]
struct DeploymentRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    latest_stage: Stage,
}

#[derive(serde::Deserialize, Default)]
struct Stage {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "result": [
            {"id":"dep-fail-1","latest_stage":{"name":"deploy","status":"failure"}},
            {"id":"dep-ok-2","latest_stage":{"name":"deploy","status":"success"}}
        ]
    }"#;

    fn cfg() -> SourceConfig {
        let mut cfg = SourceConfig::for_kind(SourceKind::CloudflareDeployments);
        cfg.max_items = 5;
        cfg.params.insert("account_id".into(), "acct-1".into());
        cfg.params.insert("pages_project".into(), "gaveta-web".into());
        cfg
    }

    fn url() -> String {
        // Built to match exactly what poll constructs so the mock keys on it.
        String::from(
            "https://api.cloudflare.com/client/v4/accounts/acct-1/pages/projects/gaveta-web/deployments?per_page=5",
        )
    }

    #[test]
    fn surfaces_only_failed_deployments() {
        let env = MockEnvironment::new()
            .roots("/code", "/home/op")
            .secret_val("cloudflare/api-token", "tok")
            .http(&url(), FIXTURE);
        let PollOutcome::Fetched(out) = CloudflareDeploymentsSource.poll(&env, &cfg()) else {
            panic!("an observed response is Fetched");
        };
        assert_eq!(out.len(), 1, "only the failed deployment surfaces");
        let d = &out[0];
        assert!(d.title.contains("gaveta-web") && d.title.contains("deploy failed"));
        assert_eq!(d.detail.as_deref(), Some("dep-fail-1"));
        assert_eq!(d.urgency, Urgency::High);
        assert_eq!(d.spawn.cwd().to_str().unwrap(), "/code");
        // The session name carries the source emoji; the title stays plain.
        assert!(d.spawn.name().starts_with('\u{1F310}'));
    }

    #[test]
    fn honesty_tiers_are_typed_not_empty() {
        // No account/project params → Unconfigured (needs config, not "green").
        let env = MockEnvironment::new();
        let bare = SourceConfig::for_kind(SourceKind::CloudflareDeployments);
        assert_eq!(
            CloudflareDeploymentsSource.poll(&env, &bare),
            PollOutcome::unconfigured()
        );
        // Params present but the token secret is missing → AuthMissing.
        assert_eq!(
            CloudflareDeploymentsSource.poll(&env, &cfg()),
            PollOutcome::auth_missing()
        );
        // Params + token present but the fetch fails → Error (keep last rows).
        let env = MockEnvironment::new().secret_val("cloudflare/api-token", "tok");
        assert_eq!(
            CloudflareDeploymentsSource.poll(&env, &cfg()),
            PollOutcome::error()
        );
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", "proj", &MockEnvironment::new(), 5).is_empty());
    }
}
