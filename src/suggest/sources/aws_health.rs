//! `aws-health` — open + upcoming AWS Health events for the account, surfaced
//! as "go look at this" suggestions. Local CLI, no extra credential beyond the
//! ambient `aws` profile/role.
//!
//! Live wiring: `aws health describe-events --filter
//! eventStatusCodes=open,upcoming --output json --region us-east-1` → an object
//! `{events: [{arn, service, eventTypeCode, statusCode}]}`. Each event becomes
//! a Critical suggestion dropping you in the code root. `aws` not installed /
//! unauthed → graceful empty.

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct AwsHealthSource;

impl SuggestionSource for AwsHealthSource {
    fn kind(&self) -> SourceKind {
        SourceKind::AwsHealth
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let cmd = Cmd::new("aws")
            .arg("health")
            .arg("describe-events")
            .arg("--filter")
            .arg("eventStatusCodes=open,upcoming")
            .arg("--output")
            .arg("json")
            .arg("--region")
            .arg("us-east-1");
        let Some(out) = env.run(&cmd) else {
            return Vec::new();
        };
        let mut items = parse(&out, env);
        items.truncate(cfg.max_items.max(1));
        items
    }
}

/// Parse `aws health describe-events --output json` into one suggestion per
/// open/upcoming event. Pure — the unit the source is tested through.
fn parse(json: &str, env: &dyn SuggestionEnvironment) -> Vec<Suggestion> {
    let Ok(envelope) = serde_json::from_str::<HealthEnvelope>(json) else {
        return Vec::new();
    };
    envelope
        .events
        .into_iter()
        .filter_map(|event| {
            let cwd = env.code_root();
            let mut name = String::from("\u{2601} "); // ☁
            name.push_str(&event.service.chars().take(20).collect::<String>());
            let spawn = SpawnSpec::new(cwd, name)?;
            let mut title = event.service.clone();
            title.push_str(": ");
            title.push_str(&event.event_type_code);
            Some(
                Suggestion::new(SourceKind::AwsHealth, &event.arn, title, spawn)
                    .detail(event.status_code)
                    .urgent(Urgency::Critical),
            )
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct HealthEnvelope {
    #[serde(default)]
    events: Vec<EventRow>,
}

#[derive(serde::Deserialize, Default)]
struct EventRow {
    #[serde(default)]
    arn: String,
    #[serde(default)]
    service: String,
    #[serde(default, rename = "eventTypeCode")]
    event_type_code: String,
    #[serde(default, rename = "statusCode")]
    status_code: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = r#"{
        "events": [
            {"arn":"arn:aws:health::event/EC2/abc","service":"EC2","eventTypeCode":"AWS_EC2_INSTANCE_STORE_DRIVE_PERFORMANCE_DEGRADED","statusCode":"open"},
            {"arn":"arn:aws:health::event/RDS/xyz","service":"RDS","eventTypeCode":"AWS_RDS_MAINTENANCE_SCHEDULED","statusCode":"upcoming"}
        ]
    }"#;

    fn env() -> MockEnvironment {
        MockEnvironment::new().roots("/code", "/home/op").cmd(
            "aws health describe-events --filter eventStatusCodes=open,upcoming --output json --region us-east-1",
            FIXTURE,
        )
    }

    #[test]
    fn produces_one_critical_suggestion_per_event() {
        let cfg = SourceConfig::for_kind(SourceKind::AwsHealth);
        let out = AwsHealthSource.poll(&env(), &cfg);
        assert_eq!(out.len(), 2);
        let ec2 = out.iter().find(|s| s.title.starts_with("EC2: ")).unwrap();
        assert!(ec2.title.contains("INSTANCE_STORE_DRIVE_PERFORMANCE_DEGRADED"));
        assert_eq!(ec2.urgency, Urgency::Critical);
        assert_eq!(ec2.detail.as_deref(), Some("open"));
        assert_eq!(ec2.spawn.cwd().to_str().unwrap(), "/code");
        let rds = out.iter().find(|s| s.title.starts_with("RDS: ")).unwrap();
        assert_eq!(rds.detail.as_deref(), Some("upcoming"));
    }

    #[test]
    fn respects_max_items() {
        let mut cfg = SourceConfig::for_kind(SourceKind::AwsHealth);
        cfg.max_items = 1;
        let out = AwsHealthSource.poll(&env(), &cfg);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn unauthed_aws_yields_nothing() {
        // No fixture registered → run() returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::AwsHealth);
        assert!(AwsHealthSource.poll(&MockEnvironment::new(), &cfg).is_empty());
    }

    #[test]
    fn garbage_json_is_safe() {
        assert!(parse("not json", &MockEnvironment::new()).is_empty());
    }
}
