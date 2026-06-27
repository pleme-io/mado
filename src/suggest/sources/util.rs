//! Shared provider primitives — the small, slick building blocks every source
//! stands on, so adding a source is a few typed lines, not copy-paste.
//!
//! The fan-out that birthed the 27 providers left each one with its own
//! `pct` / `truncate` / `basename` and the same fetch→parse boilerplate.
//! Duplication is a bug (the prime directive): a percent-encoder copied four
//! ways is four chances to diverge. These are the one home.

use serde::de::DeserializeOwned;

use crate::suggest::core::Rank;
use crate::suggest::env::{Cmd, HttpReq, SuggestionEnvironment};

/// A source's priority/severity vocabulary, normalized to a [`Rank`] on the one
/// shared scale. The trait both ranked-source families implement, so each maps
/// its own words — Jira's `Highest/High/…`, an incident's `P1`/`critical`/
/// `warning` — onto the same ladder, applied uniformly at the build site via
/// [`Suggestion::ranked`](crate::suggest::core::Suggestion::ranked). One typed
/// surface replaces the per-source ad-hoc `(urgency, score)` tuples.
pub trait PriorityScale: Sized {
    /// Parse a source's priority/severity name (case-insensitive, alias-aware).
    fn parse(name: &str) -> Self;
    /// The normalized [`Rank`] this level contributes.
    fn rank(self) -> Rank;
    /// Parse + rank in one call — the form sources use at the build site
    /// (`.ranked(JiraPriority::rank_of(p))`).
    #[must_use]
    fn rank_of(name: &str) -> Rank {
        Self::parse(name).rank()
    }
}

/// A Jira issue priority, parsed from the API's free-text priority name.
///
/// Operator directive: *"bring high priority jira tickets to the absolute top in
/// terms of generating sessions."* So [`PriorityScale::rank`] lifts **Highest**
/// and **High** into the **Critical** tier (the top of the stream), Highest
/// above High, while Medium/Low/Lowest stay calm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JiraPriority {
    Highest,
    High,
    Medium,
    Low,
    Lowest,
    /// Unrecognized / absent — ranked as ordinary queued work.
    Unknown,
}

impl PriorityScale for JiraPriority {
    /// Handles the default scheme (Highest/High/Medium/Low/Lowest), the legacy
    /// Blocker/Critical/Major/Minor/Trivial scheme, and `P0`–`P5`.
    fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "highest" | "blocker" | "p0" | "p1" => Self::Highest,
            "high" | "critical" | "major" | "urgent" => Self::High,
            "medium" | "normal" | "moderate" | "p2" => Self::Medium,
            "low" | "minor" | "p3" => Self::Low,
            "lowest" | "trivial" | "p4" | "p5" => Self::Lowest,
            _ => Self::Unknown,
        }
    }

    fn rank(self) -> Rank {
        match self {
            Self::Highest => Rank::critical_top(),
            Self::High => Rank::critical(),
            Self::Medium | Self::Unknown => Rank::normal(),
            Self::Low => Rank::low(),
            Self::Lowest => Rank::lowest(),
        }
    }
}

/// An incident/alert severity (or priority), the alert-domain sibling of
/// [`JiraPriority`]. Firing alerts default to the Critical tier, but a
/// `warning`/`P3` is NOT as urgent as a `critical`/`P1`: this orders them
/// within the stream instead of treating every firing thing identically. An
/// unrecognized/absent level keeps a firing-but-unlabeled alert urgent
/// (`Critical`), so a source with no severity data is unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IncidentSeverity {
    Critical,
    Error,
    Warning,
    Info,
    Debug,
    /// Unrecognized / absent — firing-but-unlabeled, stays urgent.
    Unknown,
}

impl PriorityScale for IncidentSeverity {
    /// Covers the common severity vocab (critical/error/warning/info/debug),
    /// `P1`–`P5`, and `SevN`.
    fn parse(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "critical" | "crit" | "fatal" | "emergency" | "page" | "p1" | "sev1" | "sev-1" => {
                Self::Critical
            }
            "error" | "err" | "high" | "p2" | "sev2" | "sev-2" => Self::Error,
            "warning" | "warn" | "medium" | "p3" | "sev3" | "sev-3" => Self::Warning,
            "info" | "informational" | "notice" | "low" | "p4" | "sev4" | "sev-4" => Self::Info,
            "debug" | "trace" | "p5" | "sev5" | "sev-5" => Self::Debug,
            _ => Self::Unknown,
        }
    }

    fn rank(self) -> Rank {
        match self {
            Self::Critical => Rank::critical_top(),
            // Error + an unlabeled-but-firing alert both sit at the Critical tier.
            Self::Error | Self::Unknown => Rank::critical(),
            Self::Warning => Rank::high(),
            Self::Info => Rank::normal(),
            Self::Debug => Rank::low(),
        }
    }
}

/// Percent-encode a query-string value (RFC 3986: unreserved `A-Za-z0-9-_.~`
/// pass through, everything else → `%XX` uppercase). Used to build JQL / CQL /
/// search query params for the HTTP sources.
#[must_use]
pub fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => {
                out.push('%');
                out.push(hex_upper(b >> 4));
                out.push(hex_upper(b & 0x0f));
            }
        }
    }
    out
}

fn hex_upper(nibble: u8) -> char {
    char::from(if nibble < 10 {
        b'0' + nibble
    } else {
        b'A' + (nibble - 10)
    })
}

/// First `n` characters of `s` (char-safe — never splits a multi-byte glyph).
/// The slick row-label trimmer the providers share.
#[must_use]
pub fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Compact human age from a seconds-elapsed count: `now` / `5m` / `3h` / `2d` /
/// `1w`. The freshness nudge the picker stamps on a task that's been waiting,
/// and a reusable fleet primitive (any "X ago" surface can call it).
#[must_use]
pub fn relative_age(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    let (n, unit) = if secs < MIN {
        return String::from("now");
    } else if secs < HOUR {
        (secs / MIN, 'm')
    } else if secs < DAY {
        (secs / HOUR, 'h')
    } else if secs < WEEK {
        (secs / DAY, 'd')
    } else {
        (secs / WEEK, 'w')
    };
    let mut out = n.to_string();
    out.push(unit);
    out
}

/// Format a Unix timestamp (seconds, UTC) as an RFC 3339 / ISO-8601 instant
/// (`YYYY-MM-DDTHH:MM:SSZ`) with no external crate. The `timeMin` /
/// since-cursor every time-windowed API source needs (Google Calendar, any
/// "events after now" feed). Uses Howard Hinnant's civil-from-days algorithm.
#[must_use]
pub fn rfc3339_utc(unix_secs: u64) -> String {
    let days = i64::try_from(unix_secs / 86_400).unwrap_or(0);
    let rem = unix_secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    let mut out = String::with_capacity(20);
    push_int(&mut out, year, 4);
    out.push('-');
    push_int(&mut out, i64::from(month), 2);
    out.push('-');
    push_int(&mut out, i64::from(day), 2);
    out.push('T');
    push_int(&mut out, i64::try_from(h).unwrap_or(0), 2);
    out.push(':');
    push_int(&mut out, i64::try_from(m).unwrap_or(0), 2);
    out.push(':');
    push_int(&mut out, i64::try_from(s).unwrap_or(0), 2);
    out.push('Z');
    out
}

/// Zero-pad `v` (assumed non-negative here) to at least `width` digits.
fn push_int(out: &mut String, v: i64, width: usize) {
    let s = v.to_string();
    for _ in s.len()..width {
        out.push('0');
    }
    out.push_str(&s);
}

/// Civil date `(year, month, day)` from a day count since 1970-01-01
/// (Howard Hinnant, <http://howardhinnant.github.io/date_algorithms.html>).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1); // [1, 31]
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1); // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Final path component (after the last `/`); the whole string if there is no
/// separator or the trailing component is empty.
#[must_use]
pub fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// Run a typed command and parse its stdout as JSON `T`. `None` on a missing
/// binary, a non-zero exit, or unparseable output — the graceful-empty path
/// every CLI-JSON source wants. Encapsulates the `env.run(&cmd)` +
/// `serde_json::from_str` two-step into one fallible call.
#[must_use]
pub fn cmd_json<T: DeserializeOwned>(env: &dyn SuggestionEnvironment, cmd: &Cmd) -> Option<T> {
    serde_json::from_str(&env.run(cmd)?).ok()
}

/// HTTP GET and parse the 2xx body as JSON `T`. `None` on a non-2xx response,
/// a missing token, or unparseable output — the graceful-empty path every
/// HTTP-JSON source wants.
#[must_use]
pub fn http_json<T: DeserializeOwned>(env: &dyn SuggestionEnvironment, req: &HttpReq) -> Option<T> {
    serde_json::from_str(&env.http_get(req)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::env::MockEnvironment;
    use proptest::prelude::*;

    #[test]
    fn pct_keeps_unreserved_and_encodes_the_rest() {
        assert_eq!(pct("aZ09-_.~"), "aZ09-_.~");
        assert_eq!(pct("a b"), "a%20b");
        assert_eq!(pct("x=1&y=2"), "x%3D1%26y%3D2");
        assert_eq!(pct("(a)!"), "%28a%29%21");
        // Multi-byte: é = 0xC3 0xA9.
        assert_eq!(pct("é"), "%C3%A9");
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("hello", 3), "hel");
        assert_eq!(truncate("hi", 9), "hi");
        assert_eq!(truncate("🌊🔥💨", 2), "🌊🔥");
    }

    #[test]
    fn relative_age_is_compact() {
        assert_eq!(relative_age(0), "now");
        assert_eq!(relative_age(59), "now");
        assert_eq!(relative_age(60), "1m");
        assert_eq!(relative_age(125), "2m");
        assert_eq!(relative_age(2 * 3600), "2h");
        assert_eq!(relative_age(2 * 86400), "2d");
        assert_eq!(relative_age(3 * 7 * 86400), "3w");
    }

    #[test]
    fn jira_priority_lifts_high_to_the_top() {
        use crate::suggest::core::{Rank, Urgency};
        // High-priority tickets land in the Critical tier (the absolute top of
        // the suggestion stream), Highest above High.
        let hi = JiraPriority::rank_of("Highest");
        let high = JiraPriority::rank_of("High");
        assert_eq!(hi.urgency, Urgency::Critical);
        assert_eq!(high.urgency, Urgency::Critical);
        assert!(hi.score > high.score, "Highest must outscore High within Critical");
        // Calm tiers stay calm.
        assert_eq!(JiraPriority::rank_of("Medium").urgency, Urgency::Normal);
        assert_eq!(JiraPriority::rank_of("Low").urgency, Urgency::Low);
        assert_eq!(JiraPriority::rank_of("Lowest").urgency, Urgency::Low);
        // Aliases + case-insensitivity (the parse arm).
        assert_eq!(JiraPriority::parse("BLOCKER"), JiraPriority::Highest);
        assert_eq!(JiraPriority::parse("major"), JiraPriority::High);
        assert_eq!(JiraPriority::parse("P0"), JiraPriority::Highest);
        // Unknown/empty → ordinary queued work, never elevated.
        assert_eq!(JiraPriority::rank_of(""), Rank::normal());
        assert_eq!(JiraPriority::rank_of("weird"), Rank::normal());
        // The rank_key ordering a high ticket produces beats a default one —
        // applied through the typed .ranked() chokepoint.
        let high_sug = crate::suggest::core::Suggestion::new(
            crate::suggest::core::SourceKind::JiraAssigned,
            "H",
            "high ticket",
            crate::suggest::core::SpawnSpec::new("/code", "h").unwrap(),
        )
        .ranked(hi);
        let normal = crate::suggest::core::Suggestion::new(
            crate::suggest::core::SourceKind::GithubAssignedIssues,
            "N",
            "normal issue",
            crate::suggest::core::SpawnSpec::new("/code", "n").unwrap(),
        );
        assert!(
            high_sug.rank_key() > normal.rank_key(),
            "a Highest jira ticket must rank above default work"
        );
    }

    #[test]
    fn incident_severity_orders_within_the_stream() {
        use crate::suggest::core::{Rank, Urgency};
        // The high tiers stay Critical; lower severities drop a tier so a P1
        // critical outranks a P3 warning outranks a P5.
        let crit = IncidentSeverity::rank_of("critical");
        let warn = IncidentSeverity::rank_of("warning");
        let p1 = IncidentSeverity::rank_of("P1");
        let p3 = IncidentSeverity::rank_of("P3");
        let p5 = IncidentSeverity::rank_of("p5");
        assert_eq!(crit.urgency, Urgency::Critical);
        assert_eq!(p1, Rank::critical_top());
        assert_eq!(warn.urgency, Urgency::High);
        assert_eq!(p3.urgency, Urgency::High);
        assert_eq!(p5.urgency, Urgency::Low);
        // Strict ordering by rank-relevant score within/across tiers.
        assert!(crit.score > warn.score && warn.score > p5.score);
        assert!(p1.score > p3.score && p3.score > p5.score);
        // Aliases + case-insensitivity.
        assert_eq!(IncidentSeverity::parse("CRIT"), IncidentSeverity::Critical);
        assert_eq!(IncidentSeverity::parse("warn"), IncidentSeverity::Warning);
        assert_eq!(IncidentSeverity::rank_of("sev1").urgency, Urgency::Critical);
        // Unknown / empty → firing-but-unlabeled stays urgent (unchanged
        // behavior for sources with no severity data).
        assert_eq!(IncidentSeverity::rank_of("").urgency, Urgency::Critical);
        assert_eq!(IncidentSeverity::rank_of("weird").urgency, Urgency::Critical);
    }

    #[test]
    fn rfc3339_utc_known_instants() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // 1_700_000_000 → 2023-11-14T22:13:20Z (a known reference value).
        assert_eq!(rfc3339_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        // Leap-year boundary: 2024-02-29.
        assert_eq!(rfc3339_utc(1_709_208_000), "2024-02-29T12:00:00Z");
    }

    #[test]
    fn basename_takes_the_last_component() {
        assert_eq!(basename("/a/b/c"), "c");
        assert_eq!(basename("/a/b/"), "b");
        assert_eq!(basename("solo"), "solo");
        assert_eq!(basename(""), "");
    }

    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Row {
        #[serde(default)]
        n: u64,
    }

    #[test]
    fn cmd_json_parses_or_none() {
        let env = MockEnvironment::new()
            .cmd("x", r#"{"n":7}"#)
            .cmd("bad", "not json");
        assert_eq!(cmd_json::<Row>(&env, &Cmd::new("x")), Some(Row { n: 7 }));
        assert_eq!(cmd_json::<Row>(&env, &Cmd::new("bad")), None);
        assert_eq!(cmd_json::<Row>(&env, &Cmd::new("missing")), None);
    }

    #[test]
    fn http_json_parses_or_none() {
        let env = MockEnvironment::new().http("https://x", r#"{"n":3}"#);
        assert_eq!(http_json::<Row>(&env, &HttpReq::new("https://x")), Some(Row { n: 3 }));
        assert_eq!(http_json::<Row>(&env, &HttpReq::new("https://missing")), None);
    }

    /// Test-only inverse of `pct`, for the round-trip property.
    fn pct_decode(s: &str) -> Vec<u8> {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                let hi = char::from(b[i + 1]).to_digit(16);
                let lo = char::from(b[i + 2]).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push(u8::try_from(h * 16 + l).unwrap());
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        out
    }

    proptest! {
        #[test]
        fn pct_round_trips(s in ".*") {
            prop_assert_eq!(pct_decode(&pct(&s)), s.as_bytes());
        }

        #[test]
        fn pct_output_is_url_safe(s in ".*") {
            for c in pct(&s).chars() {
                prop_assert!(
                    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '%'),
                    "pct emitted an unsafe char: {c:?}"
                );
            }
        }

        #[test]
        fn truncate_bounds_len_and_is_a_prefix(s in ".*", n in 0usize..50) {
            let t = truncate(&s, n);
            prop_assert!(t.chars().count() <= n);
            prop_assert!(s.starts_with(&t));
        }

        #[test]
        fn relative_age_is_nonempty_and_now_under_a_minute(secs in 0u64..3_000_000) {
            let a = relative_age(secs);
            prop_assert!(!a.is_empty());
            if secs < 60 {
                prop_assert_eq!(a.as_str(), "now");
            }
        }
    }
}
