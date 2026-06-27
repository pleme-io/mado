//! Shared provider primitives — the small, slick building blocks every source
//! stands on, so adding a source is a few typed lines, not copy-paste.
//!
//! The fan-out that birthed the 27 providers left each one with its own
//! `pct` / `truncate` / `basename` and the same fetch→parse boilerplate.
//! Duplication is a bug (the prime directive): a percent-encoder copied four
//! ways is four chances to diverge. These are the one home.

use serde::de::DeserializeOwned;

use crate::suggest::env::{Cmd, HttpReq, SuggestionEnvironment};

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
}
