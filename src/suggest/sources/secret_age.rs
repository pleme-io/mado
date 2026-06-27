//! `secret-age` — long-lived credentials under `~/.config` that look stale and
//! want rotating. Fully local (`find`), no auth, no network. Each match becomes
//! a "go rotate this" suggestion that drops you in the code root.
//!
//! Live wiring: `find ~/.config -maxdepth 2 -type f -name '*token*' -mtime +90`
//! — one path per line. The `-mtime +90` window is a coarse age proxy (file
//! mtime, not a true secret-age stat; a stat-based check is a follow-up). `find`
//! not installed / no output → no suggestions (graceful).

use crate::suggest::core::{SourceKind, SpawnSpec, Suggestion, Urgency};
use crate::suggest::env::{Cmd, SuggestionEnvironment};
use crate::suggest::source::{SourceConfig, SuggestionSource};

pub struct SecretAgeSource;

impl SuggestionSource for SecretAgeSource {
    fn kind(&self) -> SourceKind {
        SourceKind::SecretAge
    }

    fn poll(&self, env: &dyn SuggestionEnvironment, cfg: &SourceConfig) -> Vec<Suggestion> {
        let cfgdir = env.home().join(".config").to_string_lossy().into_owned();
        let cmd = Cmd::new("find")
            .arg(cfgdir)
            .arg("-maxdepth")
            .arg("2")
            .arg("-type")
            .arg("f")
            .arg("-name")
            .arg("*token*")
            .arg("-mtime")
            .arg("+90");
        let Some(out) = env.run(&cmd) else {
            return Vec::new();
        };
        parse(&out, env, cfg.max_items.max(1))
    }
}

/// Parse `find … -name '*token*' -mtime +90` output (one path per line) into
/// rotate suggestions, capped at `max`. Pure — the unit the source is tested
/// through.
fn parse(out: &str, env: &dyn SuggestionEnvironment, max: usize) -> Vec<Suggestion> {
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(max)
        .filter_map(|path| {
            let cwd = env.code_root();
            let spawn = SpawnSpec::new(cwd, "\u{1F511} rotate")?; // 🔑
            let base: String = basename(path).chars().take(64).collect();
            let mut title = String::from("rotate ");
            title.push_str(&base);
            Some(
                Suggestion::new(SourceKind::SecretAge, path, title, spawn)
                    .detail(path.to_string())
                    .urgent(Urgency::Normal),
            )
        })
        .collect()
}

use super::util::basename;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;

    const FIXTURE: &str = "/home/op/.config/github/token\n/home/op/.config/akeyless/access-token\n";

    fn env() -> MockEnvironment {
        MockEnvironment::new().roots("/code", "/home/op").cmd(
            "find /home/op/.config -maxdepth 2 -type f -name *token* -mtime +90",
            FIXTURE,
        )
    }

    #[test]
    fn produces_a_rotate_suggestion_per_stale_token() {
        let cfg = SourceConfig::for_kind(SourceKind::SecretAge);
        let out = SecretAgeSource.poll(&env(), &cfg);
        assert_eq!(out.len(), 2);
        let gh = out
            .iter()
            .find(|s| s.detail.as_deref() == Some("/home/op/.config/github/token"))
            .unwrap();
        assert!(gh.title.contains("rotate token"));
        assert_eq!(gh.urgency, Urgency::Normal);
        // Every rotate suggestion drops you in the code root.
        assert_eq!(gh.spawn.cwd().to_str().unwrap(), "/code");
    }

    #[test]
    fn respects_max_items_cap() {
        let mut cfg = SourceConfig::for_kind(SourceKind::SecretAge);
        cfg.max_items = 1;
        let out = SecretAgeSource.poll(&env(), &cfg);
        assert_eq!(out.len(), 1, "capped at max_items");
    }

    #[test]
    fn no_find_yields_nothing() {
        // No fixture registered → run() returns None → empty.
        let cfg = SourceConfig::for_kind(SourceKind::SecretAge);
        assert!(SecretAgeSource.poll(&MockEnvironment::new(), &cfg).is_empty());
    }
}
