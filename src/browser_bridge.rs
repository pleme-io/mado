//! The browser command sink + process-global bridge — how the kaname MCP and
//! the vigy (tatara-lisp) surfaces drive the LIVE GUI's floating browser
//! surfaces, and how the read-only verbs observe them.
//!
//! mado already has TWO independent GUI-injection sinks — [`crate::action_injection::InjectedActions`]
//! (chords → `Action`) and the session-switch sink — precisely because each
//! carries a distinct payload. Browser control carries a THIRD payload shape
//! (urls, ids, zones, geometry), so it gets a third sink of the same shape —
//! **NOT** a data-carrying arm on the `Copy` + `KindStr` all-unit `Action` enum
//! (that would break both derives and ripple `Copy` fleet-wide).
//!
//! Two directions:
//! - **Write** ([`BrowserCommands`]): MCP/vigy `push` a [`BrowserVerb`]; the GUI
//!   event loop `attach_sink()`s + `drain()`s per tick. A push with no live
//!   drainer is *refused* (`no-injection-sink` honesty) — never a silent lie.
//! - **Read** ([`BrowserBridge::surfaces`]): the GUI `publish`es a snapshot of
//!   its float z-stack each tick; the read-only verbs (`browser_list`) read it
//!   without reaching into the GUI.
//!
//! The process-global [`OnceLock`] mirrors `vigy_host::HOST` — installed once at
//! GUI startup, `get()` from the MCP/vigy closures.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use mado::float::BrowserId;

/// A browser control command destined for the live GUI's float manager. The
/// injection payload for the write sink (the shape-B decision — see module doc).
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserVerb {
    /// Mint a new floating surface + navigate to `url`.
    Open { url: String },
    /// Navigate an existing surface.
    Navigate { id: BrowserId, url: String },
    /// Snap a surface to a named zone (a `float::BUILTIN_ZONE_NAMES` entry).
    Snap { id: BrowserId, zone: String },
    /// Free-float a surface to absolute px.
    Move { id: BrowserId, x: f64, y: f64 },
    /// Resize a surface to absolute px.
    Resize { id: BrowserId, w: f64, h: f64 },
    /// Raise + focus a surface.
    Focus { id: BrowserId },
    /// Close a surface (+ its bound tear session, GUI-side).
    Close { id: BrowserId },
    /// Render the surface's current page to a PNG + publish it (base64) to the
    /// bridge's snapshot store, where `browser_snapshot_get` polls for it. Needs
    /// the GPU, so it's realized on the render tick (not in `apply_browser_verb`).
    Snapshot { id: BrowserId },
    /// Replace a surface's DOM from a tatara-lisp S-expression — the write side
    /// of the DOM-Way loop (the inverse of the `dom_sexp` read). The sexp is
    /// validated at the MCP/vigy border before this verb is pushed.
    SetDom { id: BrowserId, sexp: String },
}

/// The write sink — an `Arc<Mutex<VecDeque<BrowserVerb>>>` + an attached flag.
/// Cloneable (all clones share the same queue). Mirrors `InjectedActions`.
#[derive(Debug, Clone, Default)]
pub struct BrowserCommands {
    queue: Arc<Mutex<VecDeque<BrowserVerb>>>,
    sink_attached: Arc<AtomicBool>,
}

impl BrowserCommands {
    /// A fresh, detached sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The GUI event loop calls this once when it begins draining.
    pub fn attach_sink(&self) {
        self.sink_attached.store(true, Ordering::SeqCst);
    }

    /// The GUI event loop calls this when it stops draining (teardown).
    pub fn detach_sink(&self) {
        self.sink_attached.store(false, Ordering::SeqCst);
    }

    /// Whether a live GUI is draining this sink.
    #[must_use]
    pub fn sink_attached(&self) -> bool {
        self.sink_attached.load(Ordering::SeqCst)
    }

    /// Push a command (MCP/vigy side). Returns `false` (refused) when no live
    /// GUI is draining — the caller reports `no-injection-sink`, never success.
    pub fn push(&self, verb: BrowserVerb) -> bool {
        if !self.sink_attached() {
            return false;
        }
        self.queue.lock().unwrap().push_back(verb);
        true
    }

    /// Drain every pending command (GUI side, once per tick).
    #[must_use]
    pub fn drain(&self) -> Vec<BrowserVerb> {
        self.queue.lock().unwrap().drain(..).collect()
    }

    /// How many commands are queued (diagnostics/tests).
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

/// A read-only snapshot row of one live floating surface — what `browser_list`
/// returns. Serialized to JSON on the MCP/lisp boundary.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BrowserSurfaceInfo {
    /// The surface id (a `BrowserId` u32).
    pub id: u32,
    /// The current URL (empty until the first navigate).
    pub url: String,
    /// On-screen rect.
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Stacking order (higher = nearer the viewer).
    pub z: u16,
    /// Whether this surface owns the keyboard.
    pub focused: bool,
    /// Coarse interaction mode (a `FloatMode` slug).
    pub mode: String,
    /// Page load state (a `LoadState` slug).
    pub load_state: String,
    /// The page DOM as a homoiconic tatara-lisp S-expression (`dom_to_sexp`) —
    /// the queryable read side of "The DOM Way of the Browser".
    pub dom_sexp: String,
}

/// The process-global bridge: the write sink + the published read snapshot.
#[derive(Debug, Clone, Default)]
pub struct BrowserBridge {
    /// The command write sink.
    pub commands: BrowserCommands,
    surfaces: Arc<Mutex<Vec<BrowserSurfaceInfo>>>,
    /// Completed page snapshots, keyed by surface id — a base64 PNG each. The
    /// GUI `put_snapshot`s after rendering a `Snapshot` verb; the MCP
    /// `browser_snapshot_get` `take_snapshot`s (remove-on-read, one-shot).
    snapshots: Arc<Mutex<HashMap<u32, String>>>,
    /// Surface ids awaiting a GPU render into a snapshot. The engine records one
    /// here when it drains a `Snapshot` verb (it has no GPU); the render pass
    /// (`draw_float_panels`, which has the GPU) drains + renders them. The
    /// engine→renderer hop of the snapshot pipeline.
    render_reqs: Arc<Mutex<Vec<u32>>>,
}

impl BrowserBridge {
    /// A fresh bridge.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a live GUI is draining the command sink.
    #[must_use]
    pub fn sink_attached(&self) -> bool {
        self.commands.sink_attached()
    }

    /// GUI-side: publish the current float z-stack snapshot (once per tick).
    pub fn publish(&self, surfaces: Vec<BrowserSurfaceInfo>) {
        *self.surfaces.lock().unwrap() = surfaces;
    }

    /// Reader-side: the last published surface snapshot.
    #[must_use]
    pub fn surfaces(&self) -> Vec<BrowserSurfaceInfo> {
        self.surfaces.lock().unwrap().clone()
    }

    // ── Thin push helpers the MCP/vigy verbs call (return false = refused) ──

    /// Mint + open a surface at `url`.
    pub fn open(&self, url: &str) -> bool {
        self.commands.push(BrowserVerb::Open { url: url.to_owned() })
    }

    /// Navigate a surface.
    pub fn navigate(&self, id: BrowserId, url: &str) -> bool {
        self.commands.push(BrowserVerb::Navigate {
            id,
            url: url.to_owned(),
        })
    }

    /// Snap a surface to a named zone.
    pub fn snap(&self, id: BrowserId, zone: &str) -> bool {
        self.commands.push(BrowserVerb::Snap {
            id,
            zone: zone.to_owned(),
        })
    }

    /// Raise + focus a surface.
    pub fn focus(&self, id: BrowserId) -> bool {
        self.commands.push(BrowserVerb::Focus { id })
    }

    /// Close a surface.
    pub fn close(&self, id: BrowserId) -> bool {
        self.commands.push(BrowserVerb::Close { id })
    }

    /// Free-float a surface to an absolute top-left (px), clamped to the viewport.
    pub fn move_to(&self, id: BrowserId, x: f64, y: f64) -> bool {
        self.commands.push(BrowserVerb::Move { id, x, y })
    }

    /// Resize a surface to absolute px, clamped to the viewport.
    pub fn resize(&self, id: BrowserId, w: f64, h: f64) -> bool {
        self.commands.push(BrowserVerb::Resize { id, w, h })
    }

    /// Replace a surface's DOM from a tatara-lisp S-expression (the DOM-Way
    /// write side). The sexp is validated at the caller (border) before push.
    pub fn set_dom(&self, id: BrowserId, sexp: &str) -> bool {
        self.commands.push(BrowserVerb::SetDom {
            id,
            sexp: sexp.to_owned(),
        })
    }

    /// Request a page snapshot (rendered + published a tick later).
    pub fn snapshot(&self, id: BrowserId) -> bool {
        self.commands.push(BrowserVerb::Snapshot { id })
    }

    /// GUI-side: publish a rendered page snapshot (base64 PNG) for a surface.
    pub fn put_snapshot(&self, id: u32, png_base64: String) {
        self.snapshots.lock().unwrap().insert(id, png_base64);
    }

    /// Reader-side: take (remove-on-read) a published snapshot, if ready. The
    /// one-shot semantics mean a poll returns each render exactly once — a stale
    /// PNG never lingers under an id.
    #[must_use]
    pub fn take_snapshot(&self, id: u32) -> Option<String> {
        self.snapshots.lock().unwrap().remove(&id)
    }

    /// Engine-side: record a surface id awaiting a GPU snapshot render.
    pub fn push_render_req(&self, id: u32) {
        self.render_reqs.lock().unwrap().push(id);
    }

    /// Render-side: take every pending snapshot render request (drained each
    /// render pass).
    #[must_use]
    pub fn take_render_reqs(&self) -> Vec<u32> {
        std::mem::take(&mut *self.render_reqs.lock().unwrap())
    }
}

/// The process-global bridge (mirrors `vigy_host::HOST`).
static BRIDGE: OnceLock<BrowserBridge> = OnceLock::new();

/// Install the process-global bridge at GUI startup. Idempotent (the first
/// install wins; a second is a no-op).
pub fn install(bridge: BrowserBridge) {
    let _ = BRIDGE.set(bridge);
}

/// The process-global bridge, if installed (the MCP/vigy closures call this).
#[must_use]
pub fn get() -> Option<&'static BrowserBridge> {
    BRIDGE.get()
}

/// Get the process-global bridge, installing a fresh one on first call, and
/// (re-)attach its command sink. The GUI's per-frame tick calls this so the
/// sink is live whenever the GUI is drawing — an idempotent ensure.
pub fn ensure() -> &'static BrowserBridge {
    let bridge = BRIDGE.get_or_init(BrowserBridge::new);
    bridge.commands.attach_sink();
    bridge
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_push_with_no_drainer_is_refused_never_silently_queued() {
        let cmds = BrowserCommands::new();
        assert!(!cmds.sink_attached());
        assert!(!cmds.push(BrowserVerb::Open {
            url: "https://a.test/".to_owned()
        }));
        assert_eq!(cmds.pending(), 0); // refused, not queued
    }

    #[test]
    fn attach_then_push_then_drain_round_trips() {
        let cmds = BrowserCommands::new();
        cmds.attach_sink();
        assert!(cmds.push(BrowserVerb::Focus { id: BrowserId(3) }));
        assert!(cmds.push(BrowserVerb::Close { id: BrowserId(3) }));
        assert_eq!(cmds.pending(), 2);
        let drained = cmds.drain();
        assert_eq!(
            drained,
            vec![
                BrowserVerb::Focus { id: BrowserId(3) },
                BrowserVerb::Close { id: BrowserId(3) },
            ]
        );
        assert_eq!(cmds.pending(), 0);
        assert!(cmds.drain().is_empty());
    }

    #[test]
    fn detach_re_refuses_pushes() {
        let cmds = BrowserCommands::new();
        cmds.attach_sink();
        assert!(cmds.push(BrowserVerb::Focus { id: BrowserId(1) }));
        let _ = cmds.drain();
        cmds.detach_sink();
        assert!(!cmds.push(BrowserVerb::Focus { id: BrowserId(1) }));
    }

    #[test]
    fn bridge_helpers_refuse_without_a_sink_and_push_with_one() {
        let bridge = BrowserBridge::new();
        assert!(!bridge.open("https://a.test/")); // no drainer
        bridge.commands.attach_sink();
        assert!(bridge.open("https://a.test/"));
        assert!(bridge.navigate(BrowserId(0), "https://b.test/"));
        assert!(bridge.snap(BrowserId(0), "left-half"));
        let drained = bridge.commands.drain();
        assert_eq!(drained.len(), 3);
        assert_eq!(
            drained[0],
            BrowserVerb::Open {
                url: "https://a.test/".to_owned()
            }
        );
    }

    #[test]
    fn surface_snapshot_publish_read_round_trips_and_serializes() {
        let bridge = BrowserBridge::new();
        assert!(bridge.surfaces().is_empty());
        let rows = vec![BrowserSurfaceInfo {
            id: 2,
            url: "https://a.test/".to_owned(),
            x: 10.0,
            y: 20.0,
            w: 900.0,
            h: 640.0,
            z: 5,
            focused: true,
            mode: "floating".to_owned(),
            load_state: "loaded".to_owned(),
            dom_sexp: "(html (body))".to_owned(),
        }];
        bridge.publish(rows.clone());
        assert_eq!(bridge.surfaces(), rows);
        // Serializes to JSON for the MCP/lisp boundary.
        let json = serde_json::to_string(&bridge.surfaces()).expect("json");
        assert!(json.contains("\"focused\":true"));
        assert!(json.contains("left-half") == false && json.contains("floating"));
    }
}
