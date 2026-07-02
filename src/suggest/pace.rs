//! Per-host upstream pacing for the suggestion watchers — the samba
//! `LeakyBucket` light core (default-features = false: no NATS, no hyper,
//! no prometheus) applied as an in-process pacer, the same shape magma's
//! apply engine and tend already consume.
//!
//! Why: the 28-source engine fans out over a handful of upstream hosts
//! (api.github.com via `gh`, the Atlassian cloud, grafana, VM/Prom, …).
//! Un-paced, a freshness nudge or boot burst can slam one host with a
//! dozen near-simultaneous requests — exactly the shape that earns 429s
//! and secondary rate limits. One gentle bucket per host (1 rps
//! sustained, burst 3, ±10 % jitter) keeps every watcher polite without
//! any per-source code; a 429/403 flips the host into a 60 s cooldown
//! that short-circuits calls to `None`, which flows through the existing
//! [`PollOutcome`](super::source::PollOutcome) honesty border as
//! `Unavailable(Error)` — last-known rows stay on the board.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The synthetic host every `gh` CLI invocation is billed against — the
/// CLI talks to the GitHub REST/GraphQL API even though the watcher never
/// sees a URL.
pub const GH_HOST: &str = "api.github.com";

/// Sustained request rate per host (requests/second).
const RPS: f64 = 1.0;
/// Burst capacity per host.
const BURST: u32 = 3;
/// Cooldown after an upstream 429 (or 403 on the GitHub host).
const COOLDOWN: Duration = Duration::from_secs(60);

/// Admission verdict for one call against one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Token acquired (possibly after a paced wait) — proceed.
    Proceed,
    /// The host is inside a rate-limit cooldown — skip the call.
    CoolingDown,
}

/// One samba bucket + cooldown gate per upstream host.
pub struct HostPacer {
    buckets: parking_lot::Mutex<BTreeMap<String, Arc<samba::LeakyBucket>>>,
    cooldown: parking_lot::Mutex<BTreeMap<String, Instant>>,
}

impl HostPacer {
    /// The gentle profile: 1 rps sustained / burst 3 / ±10 % jitter —
    /// initial_rph 3600 with quota 1.0 is exactly 1 request/second.
    #[must_use]
    pub fn gentle() -> Self {
        Self {
            buckets: parking_lot::Mutex::new(BTreeMap::new()),
            cooldown: parking_lot::Mutex::new(BTreeMap::new()),
        }
    }

    /// Admit one call against `host`. Cooldown is checked FIRST — a
    /// cooled call must not consume a token or block. The blocking
    /// bucket acquire happens OUTSIDE the map lock. Legal in the sync
    /// environment trait because every source poll runs on the suggest
    /// runtime's blocking pool, where `Handle::block_on` is permitted;
    /// outside any runtime (bare unit tests) pacing is skipped.
    pub fn admit(&self, host: &str) -> Admit {
        if let Some(&until) = self.cooldown.lock().get(host) {
            if Instant::now() < until {
                return Admit::CoolingDown;
            }
        }
        let bucket = {
            let mut map = self.buckets.lock();
            map.entry(host.to_owned())
                .or_insert_with(|| {
                    Arc::new(
                        samba::LeakyBucket::new(1.0, RPS * 3600.0, 50, 25, 0.10, BURST)
                            .expect("static bucket parameters are valid"),
                    )
                })
                .clone()
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let waited = handle.block_on(bucket.acquire());
            if waited > Duration::from_millis(250) {
                tracing::debug!(host, waited_ms = waited.as_millis() as u64, "paced upstream call");
            }
        }
        Admit::Proceed
    }

    /// Record an upstream rate-limit signal: every call against `host`
    /// short-circuits to [`Admit::CoolingDown`] for the next 60 s.
    pub fn report_rate_limited(&self, host: &str, status: u16) {
        self.cooldown
            .lock()
            .insert(host.to_owned(), Instant::now() + COOLDOWN);
        tracing::warn!(host, status, "upstream rate-limited; cooling down 60s");
    }
}

/// The host of an http(s) URL — pure string slicing, no url crate.
/// `None` for non-URL strings (those calls go unpaced).
#[must_use]
pub fn host_of(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let end = rest
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let host_port = &rest[..end];
    // Strip userinfo and port; keep it allocation-free.
    let host_port = host_port.rsplit('@').next().unwrap_or(host_port);
    let host = host_port.split(':').next().unwrap_or(host_port);
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_extracts_hosts() {
        assert_eq!(host_of("https://api.github.com/repos/x"), Some("api.github.com"));
        assert_eq!(host_of("http://grafana.quero.cloud:3000/api"), Some("grafana.quero.cloud"));
        assert_eq!(host_of("https://user:pw@example.com/x?y#z"), Some("example.com"));
        assert_eq!(host_of("https://plain.host"), Some("plain.host"));
        assert_eq!(host_of("not-a-url"), None);
        assert_eq!(host_of("https://"), None);
    }

    /// Outside a tokio runtime, admit never blocks and cooldown gating
    /// still works — the full sync contract for bare tests.
    #[test]
    fn cooldown_short_circuits_and_expires() {
        let p = HostPacer::gentle();
        assert_eq!(p.admit("h.example"), Admit::Proceed);
        p.report_rate_limited("h.example", 429);
        assert_eq!(p.admit("h.example"), Admit::CoolingDown);
        // A different host is unaffected.
        assert_eq!(p.admit("other.example"), Admit::Proceed);
        // Force-expire the cooldown and verify recovery.
        p.cooldown
            .lock()
            .insert("h.example".into(), Instant::now() - Duration::from_secs(1));
        assert_eq!(p.admit("h.example"), Admit::Proceed);
    }
}
