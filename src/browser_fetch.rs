//! Off-thread page fetch for the floating browser.
//!
//! `RealBrowserBackend::navigate` does a BLOCKING `todoku` fetch; it must NOT
//! run on the render tick (a stall freezes the 60fps GUI, and `todoku`'s
//! blocking client panics inside an async context). So a `browser_open` /
//! `browser_navigate` spawns a detached OS thread that fetches the HTML bytes
//! and sends one [`FetchDone`] back over an `mpsc` channel; the engine drains
//! completed fetches each tick and `render_html`s them (fast, non-blocking) —
//! the page appears "a tick later", never a freeze.
//!
//! The `mpsc` channel IS the Environment seam (TESTING-SUBSTRATE §IX): tests
//! feed a synthetic [`FetchDone`] into the receiver — they never touch the
//! network or `todoku`.

use mado::float::BrowserId;

/// One completed fetch, sent from a worker thread back to the engine.
#[derive(Debug, Clone)]
pub struct FetchDone {
    /// The surface the fetch was for.
    pub id: BrowserId,
    /// The requested URL (for the surface's URL field).
    pub url: String,
    /// `Ok(html)` on success, `Err(display)` on a fetch/decode failure.
    pub result: Result<String, String>,
    /// The request epoch — a stale-guard: a `FetchDone` whose epoch no longer
    /// matches the surface's current epoch (a newer navigate superseded it) is
    /// dropped, so an out-of-order slow fetch never clobbers a newer page.
    pub epoch: u64,
}

/// Spawn a detached OS thread that runs the blocking fetch and sends one
/// [`FetchDone`]. Never blocks the caller; a thread-spawn failure degrades
/// (the surface keeps its loading placeholder) rather than panicking.
pub fn spawn_fetch(
    tx: std::sync::mpsc::Sender<FetchDone>,
    id: BrowserId,
    url: url::Url,
    epoch: u64,
) {
    let _ = std::thread::Builder::new()
        .name("mado-browser-fetch".to_owned())
        .spawn(move || {
            let client = nami_core::net::FetchClient::new();
            let result = match client.fetch(&url) {
                Ok(resp) => match resp.text() {
                    Some(html) => Ok(html.to_owned()),
                    None => Err("response had no UTF-8 text body".to_owned()),
                },
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(FetchDone {
                id,
                url: url.to_string(),
                result,
                epoch,
            });
        });
}

/// Apply one completed fetch to the live surface map, with the stale-epoch
/// guard. Returns `true` if it landed (surface exists AND its epoch matches),
/// `false` if it was dropped (no such surface, or a newer navigate has bumped
/// the epoch past this fetch's). Pure over the map — the only side effect is
/// re-rendering the matched surface's backend. This is the seam the drain loop
/// and the tests both call; the network never appears here.
pub fn apply_fetch_done(
    surfaces: &mut std::collections::HashMap<BrowserId, crate::browser_engine::BrowserSurface>,
    done: FetchDone,
) -> bool {
    let Some(s) = surfaces.get_mut(&done.id) else {
        return false;
    };
    if s.fetch_epoch != done.epoch {
        return false; // superseded by a newer navigate — drop it
    }
    match done.result {
        Ok(html) => {
            s.backend.render_html(&html);
            s.url = done.url;
        }
        Err(e) => s.backend.render_error(&e),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_engine::{BrowserSurface, RealBrowserBackend};
    use mado::float::BrowserBackend; // brings load_state() into scope

    fn surface(url: &str, epoch: u64) -> BrowserSurface {
        BrowserSurface {
            backend: RealBrowserBackend::nami_native(400, 300),
            url: url.to_owned(),
            fetch_epoch: epoch,
        }
    }

    #[test]
    fn a_matching_epoch_fetch_lands_and_renders() {
        let mut m = std::collections::HashMap::new();
        m.insert(BrowserId(1), surface("about:blank", 1));
        let landed = apply_fetch_done(
            &mut m,
            FetchDone {
                id: BrowserId(1),
                url: "https://example.com/".to_owned(),
                result: Ok("<style>div{background-color:#00ff00;width:80px;height:40px}</style><div></div>".to_owned()),
                epoch: 1,
            },
        );
        assert!(landed, "a matching-epoch fetch must land");
        let s = &m[&BrowserId(1)];
        assert_eq!(s.url, "https://example.com/", "url updates on land");
        assert_eq!(s.backend.load_state().as_str(), "loaded");
        assert!(
            !s.backend.current_display_list().is_empty(),
            "the landed page must paint a non-empty display list"
        );
    }

    #[test]
    fn a_stale_epoch_fetch_is_dropped() {
        let mut m = std::collections::HashMap::new();
        // Surface already advanced to epoch 3 (two navigates happened).
        m.insert(BrowserId(7), surface("https://current/", 3));
        let landed = apply_fetch_done(
            &mut m,
            FetchDone {
                id: BrowserId(7),
                url: "https://superseded/".to_owned(),
                result: Ok("<div></div>".to_owned()),
                epoch: 2, // an older in-flight fetch
            },
        );
        assert!(!landed, "a stale-epoch fetch must be dropped");
        assert_eq!(
            m[&BrowserId(7)].url,
            "https://current/",
            "a dropped fetch must not clobber the current url"
        );
    }

    #[test]
    fn a_fetch_for_a_closed_surface_is_dropped() {
        let mut m: std::collections::HashMap<BrowserId, BrowserSurface> =
            std::collections::HashMap::new();
        let landed = apply_fetch_done(
            &mut m,
            FetchDone {
                id: BrowserId(99),
                url: "https://gone/".to_owned(),
                result: Ok("<div></div>".to_owned()),
                epoch: 1,
            },
        );
        assert!(
            !landed,
            "a fetch for a closed/unknown surface must be dropped"
        );
    }

    #[test]
    fn a_failed_fetch_marks_the_surface_failed() {
        let mut m = std::collections::HashMap::new();
        m.insert(BrowserId(2), surface("https://x/", 1));
        let landed = apply_fetch_done(
            &mut m,
            FetchDone {
                id: BrowserId(2),
                url: "https://x/".to_owned(),
                result: Err("connection refused".to_owned()),
                epoch: 1,
            },
        );
        assert!(
            landed,
            "a failed fetch still lands (renders the error card)"
        );
        assert_eq!(m[&BrowserId(2)].backend.load_state().as_str(), "failed");
    }
}
