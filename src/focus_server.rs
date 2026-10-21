use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Waker;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{FocusServerConfig, FocusUnclaimed};
use crate::notify_center::NotificationCenter;
use crate::platform::WindowIdentity;
use crate::session_switch::SwitchRequests;

const MAX_LINE_BYTES: u64 = 4096;
const READ_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusMessage {
    FocusPane(String),
    Action(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusError {
    Malformed,
    EmptyPane,
    UnsupportedAction,
}

impl FocusError {
    fn code(&self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::EmptyPane => "empty-pane",
            Self::UnsupportedAction => "unsupported-action",
        }
    }
}

pub fn parse_line(line: &str) -> Result<String, FocusError> {
    match serde_json::from_str::<FocusMessage>(line.trim()) {
        Ok(FocusMessage::FocusPane(pane)) => {
            let pane = pane.trim();
            if pane.is_empty() {
                Err(FocusError::EmptyPane)
            } else {
                Ok(pane.to_owned())
            }
        }
        Ok(FocusMessage::Action(_)) => Err(FocusError::UnsupportedAction),
        Err(_) => Err(FocusError::Malformed),
    }
}

const MAX_PENDING: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimPolicy {
    pub grace: Duration,
    pub unclaimed: FocusUnclaimed,
}

impl From<&FocusServerConfig> for ClaimPolicy {
    fn from(cfg: &FocusServerConfig) -> Self {
        Self {
            grace: cfg.claim_after(),
            unclaimed: cfg.unclaimed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusPlan {
    Raise,
    SwitchThenRaise(tear_types::PaneId),
    Reject(&'static str),
}

#[must_use]
pub fn plan(requested: &str, current: &str, can_switch: bool) -> FocusPlan {
    if requested == current {
        return FocusPlan::Raise;
    }
    match requested.parse::<tear_types::PaneId>() {
        Ok(pane) if can_switch => FocusPlan::SwitchThenRaise(pane),
        Ok(_) => FocusPlan::Reject("switching-disabled"),
        Err(_) => FocusPlan::Reject("unknown-pane"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowView {
    pub slot: u64,
    pub pane: String,
    pub focus_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    NotMine,
    Wait(Instant),
    Raise,
    SwitchThenRaise(tear_types::PaneId),
    Refuse(&'static str),
}

#[must_use]
pub fn last_focused(windows: &[WindowView]) -> Option<u64> {
    windows
        .iter()
        .max_by_key(|w| (w.focus_seq, w.slot))
        .map(|w| w.slot)
}

#[must_use]
pub fn claim(
    requested: &str,
    posted: Instant,
    me: u64,
    windows: &[WindowView],
    can_switch: bool,
    policy: &ClaimPolicy,
    now: Instant,
) -> Claim {
    let Some(mine) = windows.iter().find(|w| w.slot == me) else {
        return Claim::NotMine;
    };
    if mine.pane == requested {
        return Claim::Raise;
    }
    if windows.iter().any(|w| w.pane == requested) {
        return Claim::NotMine;
    }
    let deadline = posted + policy.grace;
    if now < deadline {
        return Claim::Wait(deadline);
    }
    if last_focused(windows) != Some(me) {
        return Claim::NotMine;
    }
    match policy.unclaimed {
        FocusUnclaimed::Ignore => Claim::Refuse("unclaimed-ignored"),
        FocusUnclaimed::SwitchLastFocused => match plan(requested, &mine.pane, can_switch) {
            FocusPlan::Raise => Claim::Raise,
            FocusPlan::SwitchThenRaise(pane) => Claim::SwitchThenRaise(pane),
            FocusPlan::Reject(reason) => Claim::Refuse(reason),
        },
    }
}

#[derive(Debug, Clone)]
struct Pending {
    pane: String,
    posted: Instant,
}

struct Registered {
    view: WindowView,
    wake: Option<Waker>,
}

#[derive(Default)]
struct HubState {
    pending: Vec<Pending>,
    windows: Vec<Registered>,
    next_slot: u64,
    focus_clock: u64,
}

#[derive(Clone)]
pub struct FocusHub {
    state: Arc<Mutex<HubState>>,
    policy: ClaimPolicy,
}

impl FocusHub {
    #[must_use]
    pub fn new(policy: ClaimPolicy) -> Self {
        Self {
            state: Arc::new(Mutex::new(HubState::default())),
            policy,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn post(&self, pane: String) {
        self.post_at(pane, Instant::now());
    }

    pub fn post_at(&self, pane: String, posted: Instant) {
        let wakers = {
            let mut st = self.lock();
            st.pending.retain(|p| p.pane != pane);
            st.pending.push(Pending { pane, posted });
            if st.pending.len() > MAX_PENDING {
                st.pending.remove(0);
            }
            st.windows
                .iter()
                .filter_map(|w| w.wake.clone())
                .collect::<Vec<_>>()
        };
        for w in wakers {
            w.wake();
        }
        crate::ring::WINDOW.ring();
    }

    #[must_use]
    pub fn pending_panes(&self) -> Vec<String> {
        self.lock().pending.iter().map(|p| p.pane.clone()).collect()
    }

    #[must_use]
    pub fn register(&self, wake: Option<Waker>) -> FocusWindow {
        let mut st = self.lock();
        let slot = st.next_slot;
        st.next_slot += 1;
        st.windows.push(Registered {
            view: WindowView {
                slot,
                pane: String::new(),
                focus_seq: 0,
            },
            wake,
        });
        FocusWindow {
            hub: self.clone(),
            slot,
        }
    }

    #[must_use]
    pub fn window_count(&self) -> usize {
        self.lock().windows.len()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WindowState<'a> {
    pub pane: &'a str,
    pub focused: bool,
    pub can_switch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusAction {
    Switch(tear_types::PaneId),
    Raise,
    Refused { pane: String, reason: &'static str },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FocusServed {
    pub actions: Vec<FocusAction>,
    pub wake_at: Option<Instant>,
}

pub struct FocusWindow {
    hub: FocusHub,
    slot: u64,
}

impl Drop for FocusWindow {
    fn drop(&mut self) {
        let wakers = {
            let mut st = self.hub.lock();
            st.windows.retain(|w| w.view.slot != self.slot);
            st.windows
                .iter()
                .filter_map(|w| w.wake.clone())
                .collect::<Vec<_>>()
        };
        for w in wakers {
            w.wake();
        }
    }
}

impl FocusWindow {
    #[must_use]
    pub fn slot(&self) -> u64 {
        self.slot
    }

    pub fn service(&self, state: WindowState<'_>, now: Instant) -> FocusServed {
        let policy = self.hub.policy;
        let mut st = self.hub.lock();
        let clock = st.focus_clock;
        let bump = state.focused
            && st
                .windows
                .iter()
                .find(|w| w.view.slot == self.slot)
                .is_some_and(|w| w.view.focus_seq != clock || clock == 0);
        if bump {
            st.focus_clock += 1;
        }
        let seq = st.focus_clock;
        if let Some(me) = st.windows.iter_mut().find(|w| w.view.slot == self.slot) {
            me.view.pane.clear();
            me.view.pane.push_str(state.pane);
            if bump {
                me.view.focus_seq = seq;
            }
        }
        let views: Vec<WindowView> = st.windows.iter().map(|w| w.view.clone()).collect();
        let mut served = FocusServed::default();
        let mut keep = Vec::with_capacity(st.pending.len());
        let mut raise = false;
        for req in std::mem::take(&mut st.pending) {
            match claim(
                &req.pane,
                req.posted,
                self.slot,
                &views,
                state.can_switch,
                &policy,
                now,
            ) {
                Claim::NotMine => keep.push(req),
                Claim::Wait(at) => {
                    served.wake_at = Some(served.wake_at.map_or(at, |w| w.min(at)));
                    keep.push(req);
                }
                Claim::Raise => raise = true,
                Claim::SwitchThenRaise(pane) => {
                    served.actions.push(FocusAction::Switch(pane));
                    raise = true;
                }
                Claim::Refuse(reason) => served.actions.push(FocusAction::Refused {
                    pane: req.pane,
                    reason,
                }),
            }
        }
        st.pending = keep;
        if raise {
            served.actions.push(FocusAction::Raise);
        }
        served
    }

    pub fn drive(
        &self,
        state: WindowState<'_>,
        switch: Option<&SwitchRequests>,
        identity: Option<WindowIdentity>,
        notify: &mut NotificationCenter,
    ) -> Option<Instant> {
        let now = Instant::now();
        let served = self.service(state, now);
        for action in &served.actions {
            match action {
                FocusAction::Switch(pane) => {
                    if let Some(sw) = switch {
                        sw.post(*pane);
                    }
                }
                FocusAction::Raise => {
                    let outcome = crate::platform::raise_window(identity);
                    if identity.is_none() {
                        tracing::warn!(
                            ?outcome,
                            "focus request served before this window's identity was known"
                        );
                    }
                    notify.acknowledge(now);
                }
                FocusAction::Refused { pane, reason } => {
                    tracing::warn!(%pane, reason, "focus request not served");
                }
            }
        }
        served.wake_at
    }
}

pub struct FocusRelay {
    hub: FocusHub,
    _server: Option<FocusServer>,
}

static PROCESS_RELAY: OnceLock<FocusRelay> = OnceLock::new();

impl FocusRelay {
    #[must_use]
    pub fn start(cfg: &FocusServerConfig) -> Self {
        let hub = FocusHub::new(ClaimPolicy::from(cfg));
        let server = configured_socket(cfg).and_then(|path| {
            match spawn_server(&path, hub.clone()) {
                Ok(s) => {
                    tracing::info!(socket = %path.display(), "focus socket listening");
                    Some(s)
                }
                Err(e) => {
                    tracing::warn!(socket = %path.display(), error = %e, "focus socket unavailable");
                    None
                }
            }
        });
        Self {
            hub,
            _server: server,
        }
    }

    pub fn process(cfg: &FocusServerConfig) -> &'static Self {
        PROCESS_RELAY.get_or_init(|| Self::start(cfg))
    }

    #[must_use]
    pub fn hub(&self) -> &FocusHub {
        &self.hub
    }

    #[must_use]
    pub fn join(&self, wake: Option<Waker>) -> FocusWindow {
        self.hub.register(wake)
    }
}

pub fn handle_line(line: &str, hub: &FocusHub) -> String {
    match parse_line(line) {
        Ok(pane) => {
            hub.post(pane);
            "{\"ok\":true}\n".to_owned()
        }
        Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}\n", e.code()),
    }
}

#[must_use]
pub fn default_socket_path() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mado-focus.sock")
}

#[must_use]
pub fn configured_socket(cfg: &FocusServerConfig) -> Option<PathBuf> {
    cfg.enabled
        .then(|| cfg.socket.clone().unwrap_or_else(default_socket_path))
}

pub struct FocusServer {
    path: PathBuf,
}

impl Drop for FocusServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "another mado owns the focus socket",
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(listener)
}

fn serve_connection(stream: UnixStream, hub: &FocusHub) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut line = String::new();
    let read = BufReader::new((&stream).take(MAX_LINE_BYTES)).read_line(&mut line);
    let reply = match read {
        Ok(0) | Err(_) => return,
        Ok(_) => handle_line(&line, hub),
    };
    let _ = (&stream).write_all(reply.as_bytes());
}

pub fn spawn_server(path: &Path, hub: FocusHub) -> std::io::Result<FocusServer> {
    let listener = bind(path)?;
    let spawned = std::thread::Builder::new()
        .name("mado-focus".to_owned())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                serve_connection(stream, &hub);
            }
        });
    if let Err(e) = spawned {
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(FocusServer {
        path: path.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tear_types::PaneId;

    fn pane(seed: &str) -> String {
        PaneId::from_seed(seed).to_string()
    }

    fn policy(ms: u64, unclaimed: FocusUnclaimed) -> ClaimPolicy {
        ClaimPolicy {
            grace: Duration::from_millis(ms),
            unclaimed,
        }
    }

    fn state<'a>(pane: &'a str, focused: bool, can_switch: bool) -> WindowState<'a> {
        WindowState {
            pane,
            focused,
            can_switch,
        }
    }

    fn views(rows: &[(u64, &str, u64)]) -> Vec<WindowView> {
        rows.iter()
            .map(|(slot, pane, seq)| WindowView {
                slot: *slot,
                pane: (*pane).to_owned(),
                focus_seq: *seq,
            })
            .collect()
    }

    #[test]
    fn the_documented_line_parses() {
        assert_eq!(parse_line("{\"focus_pane\":\"ab12\"}\n"), Ok("ab12".into()));
    }

    #[test]
    fn bad_lines_are_typed_refusals() {
        assert_eq!(parse_line("nope"), Err(FocusError::Malformed));
        assert_eq!(parse_line("{\"x\":1}"), Err(FocusError::Malformed));
        assert_eq!(
            parse_line("{\"focus_pane\":\"  \"}"),
            Err(FocusError::EmptyPane)
        );
        assert_eq!(
            parse_line("{\"action\":\"copy\"}"),
            Err(FocusError::UnsupportedAction)
        );
    }

    #[test]
    fn a_valid_line_is_queued_and_a_repeat_for_the_same_pane_replaces_it() {
        let hub = FocusHub::new(policy(0, FocusUnclaimed::Ignore));
        assert_eq!(
            handle_line("{\"focus_pane\":\"a\"}", &hub),
            "{\"ok\":true}\n"
        );
        assert_eq!(
            handle_line("{\"focus_pane\":\"b\"}", &hub),
            "{\"ok\":true}\n"
        );
        assert_eq!(
            handle_line("{\"focus_pane\":\"a\"}", &hub),
            "{\"ok\":true}\n"
        );
        assert_eq!(hub.pending_panes(), vec!["b".to_owned(), "a".to_owned()]);
    }

    #[test]
    fn a_refused_line_queues_nothing() {
        let hub = FocusHub::new(policy(0, FocusUnclaimed::Ignore));
        assert_eq!(
            handle_line("{\"focus_pane\":\"\"}", &hub),
            "{\"ok\":false,\"error\":\"empty-pane\"}\n"
        );
        assert!(hub.pending_panes().is_empty());
    }

    #[test]
    fn the_queue_is_bounded() {
        let hub = FocusHub::new(policy(0, FocusUnclaimed::Ignore));
        for i in 0..(MAX_PENDING + 5) {
            hub.post(format!("p{i}"));
        }
        assert_eq!(hub.pending_panes().len(), MAX_PENDING);
        assert_eq!(hub.pending_panes()[0], "p5");
    }

    #[test]
    fn the_displayed_pane_is_raised_without_a_switch() {
        assert_eq!(plan("local", "local", false), FocusPlan::Raise);
    }

    #[test]
    fn another_pane_switches_only_when_a_switch_sink_exists() {
        let other = PaneId::from_seed("other");
        let id = other.to_string();
        assert_eq!(plan(&id, "cur", true), FocusPlan::SwitchThenRaise(other));
        assert_eq!(
            plan(&id, "cur", false),
            FocusPlan::Reject("switching-disabled")
        );
        assert_eq!(
            plan("not-a-pane", "cur", true),
            FocusPlan::Reject("unknown-pane")
        );
    }

    #[test]
    fn claim_the_window_showing_the_pane_raises_and_no_other_does() {
        let t0 = Instant::now();
        let v = views(&[(0, "a", 1), (1, "b", 2)]);
        let p = policy(300, FocusUnclaimed::SwitchLastFocused);
        assert_eq!(claim("b", t0, 1, &v, true, &p, t0), Claim::Raise);
        assert_eq!(claim("b", t0, 0, &v, true, &p, t0), Claim::NotMine);
        let later = t0 + Duration::from_secs(60);
        assert_eq!(claim("b", t0, 0, &v, true, &p, later), Claim::NotMine);
    }

    #[test]
    fn claim_an_unshown_pane_waits_the_grace_then_goes_to_the_last_focused_window() {
        let t0 = Instant::now();
        let target = PaneId::from_seed("elsewhere");
        let id = target.to_string();
        let v = views(&[(0, "a", 5), (1, "b", 2)]);
        let p = policy(300, FocusUnclaimed::SwitchLastFocused);
        let deadline = t0 + Duration::from_millis(300);
        assert_eq!(claim(&id, t0, 0, &v, true, &p, t0), Claim::Wait(deadline));
        assert_eq!(claim(&id, t0, 1, &v, true, &p, t0), Claim::Wait(deadline));
        assert_eq!(
            claim(&id, t0, 0, &v, true, &p, deadline),
            Claim::SwitchThenRaise(target)
        );
        assert_eq!(claim(&id, t0, 1, &v, true, &p, deadline), Claim::NotMine);
    }

    #[test]
    fn claim_the_ignore_policy_drops_with_a_typed_refusal() {
        let t0 = Instant::now();
        let id = PaneId::from_seed("elsewhere").to_string();
        let v = views(&[(0, "a", 1)]);
        let p = policy(0, FocusUnclaimed::Ignore);
        assert_eq!(
            claim(&id, t0, 0, &v, true, &p, t0),
            Claim::Refuse("unclaimed-ignored")
        );
    }

    #[test]
    fn claim_disabled_switching_and_unknown_panes_refuse() {
        let t0 = Instant::now();
        let id = PaneId::from_seed("elsewhere").to_string();
        let v = views(&[(0, "a", 1)]);
        let p = policy(0, FocusUnclaimed::SwitchLastFocused);
        assert_eq!(
            claim(&id, t0, 0, &v, false, &p, t0),
            Claim::Refuse("switching-disabled")
        );
        assert_eq!(
            claim("not-a-pane", t0, 0, &v, true, &p, t0),
            Claim::Refuse("unknown-pane")
        );
    }

    #[test]
    fn claim_a_window_that_is_gone_claims_nothing() {
        let t0 = Instant::now();
        let v = views(&[(0, "a", 1)]);
        let p = policy(0, FocusUnclaimed::SwitchLastFocused);
        assert_eq!(claim("a", t0, 9, &v, true, &p, t0), Claim::NotMine);
    }

    #[test]
    fn the_last_focused_window_is_the_highest_sequence_then_the_newest_slot() {
        assert_eq!(last_focused(&views(&[(0, "a", 3), (1, "b", 7)])), Some(1));
        assert_eq!(last_focused(&views(&[(0, "a", 0), (1, "b", 0)])), Some(1));
        assert_eq!(last_focused(&[]), None);
    }

    #[test]
    fn two_windows_one_relay_a_click_for_b_is_never_served_by_a() {
        let hub = FocusHub::new(policy(300, FocusUnclaimed::SwitchLastFocused));
        let a = hub.register(None);
        let b = hub.register(None);
        let (pa, pb) = (pane("a"), pane("b"));
        let t0 = Instant::now();
        assert!(a.service(state(&pa, true, true), t0).actions.is_empty());
        assert!(b.service(state(&pb, false, true), t0).actions.is_empty());
        hub.post_at(pb.clone(), t0);
        for step in [0u64, 10, 400, 5_000] {
            let served = a.service(state(&pa, true, true), t0 + Duration::from_millis(step));
            assert!(
                served.actions.is_empty(),
                "window A must never act on B's pane (step {step}): {served:?}"
            );
        }
        assert_eq!(hub.pending_panes(), vec![pb.clone()]);
        let served = b.service(state(&pb, false, true), t0 + Duration::from_millis(20));
        assert_eq!(served.actions, vec![FocusAction::Raise]);
        assert!(hub.pending_panes().is_empty());
    }

    #[test]
    fn an_unshown_pane_is_switched_by_exactly_the_last_focused_window_after_the_grace() {
        let hub = FocusHub::new(policy(300, FocusUnclaimed::SwitchLastFocused));
        let a = hub.register(None);
        let b = hub.register(None);
        let (pa, pb) = (pane("a"), pane("b"));
        let target = PaneId::from_seed("background");
        let t0 = Instant::now();
        a.service(state(&pa, true, true), t0);
        b.service(state(&pb, true, true), t0);
        hub.post_at(target.to_string(), t0);
        let early = a.service(state(&pa, false, true), t0 + Duration::from_millis(100));
        assert!(early.actions.is_empty());
        assert_eq!(early.wake_at, Some(t0 + Duration::from_millis(300)));
        let late = t0 + Duration::from_millis(301);
        assert!(a.service(state(&pa, false, true), late).actions.is_empty());
        assert_eq!(
            b.service(state(&pb, false, true), late).actions,
            vec![FocusAction::Switch(target), FocusAction::Raise]
        );
        assert!(hub.pending_panes().is_empty());
    }

    #[test]
    fn the_ignore_policy_drops_an_unshown_pane_without_touching_any_window() {
        let hub = FocusHub::new(policy(0, FocusUnclaimed::Ignore));
        let a = hub.register(None);
        let pa = pane("a");
        let t0 = Instant::now();
        a.service(state(&pa, true, true), t0);
        let target = pane("background");
        hub.post_at(target.clone(), t0);
        let served = a.service(state(&pa, true, true), t0);
        assert_eq!(
            served.actions,
            vec![FocusAction::Refused {
                pane: target,
                reason: "unclaimed-ignored"
            }]
        );
        assert!(hub.pending_panes().is_empty());
    }

    #[test]
    fn disabled_switching_refuses_instead_of_acting_on_the_wrong_window() {
        let hub = FocusHub::new(policy(0, FocusUnclaimed::SwitchLastFocused));
        let a = hub.register(None);
        let pa = pane("a");
        let t0 = Instant::now();
        a.service(state(&pa, true, false), t0);
        let target = pane("background");
        hub.post_at(target.clone(), t0);
        let served = a.service(state(&pa, true, false), t0);
        assert_eq!(
            served.actions,
            vec![FocusAction::Refused {
                pane: target,
                reason: "switching-disabled"
            }]
        );
    }

    #[test]
    fn a_closed_window_stops_counting_as_showing_its_pane() {
        let hub = FocusHub::new(policy(0, FocusUnclaimed::SwitchLastFocused));
        let a = hub.register(None);
        let b = hub.register(None);
        let (pa, pb) = (pane("a"), pane("b"));
        let t0 = Instant::now();
        a.service(state(&pa, true, true), t0);
        b.service(state(&pb, false, true), t0);
        drop(b);
        assert_eq!(hub.window_count(), 1);
        hub.post_at(pb.clone(), t0);
        let served = a.service(state(&pa, true, true), t0);
        assert_eq!(
            served.actions,
            vec![FocusAction::Switch(pb.parse().unwrap()), FocusAction::Raise]
        );
    }

    #[test]
    fn a_post_wakes_every_registered_window() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let hub = FocusHub::new(policy(0, FocusUnclaimed::Ignore));
        let c1 = Arc::new(Count(AtomicUsize::new(0)));
        let c2 = Arc::new(Count(AtomicUsize::new(0)));
        let _a = hub.register(Some(Waker::from(Arc::clone(&c1))));
        let _b = hub.register(Some(Waker::from(Arc::clone(&c2))));
        hub.post("x".into());
        assert_eq!(c1.0.load(Ordering::SeqCst), 1);
        assert_eq!(c2.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_client_that_closes_before_the_reply_does_not_stop_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.sock");
        let hub = FocusHub::new(policy(0, FocusUnclaimed::Ignore));
        let _server = spawn_server(&path, hub.clone()).unwrap();
        for pane in ["a", "b"] {
            let mut c = UnixStream::connect(&path).unwrap();
            c.write_all(format!("{{\"focus_pane\":\"{pane}\"}}\n").as_bytes())
                .unwrap();
            drop(c);
        }
        let mut c = UnixStream::connect(&path).unwrap();
        c.write_all(b"{\"focus_pane\":\"z\"}\n").unwrap();
        let mut reply = String::new();
        BufReader::new(&c).read_line(&mut reply).unwrap();
        assert_eq!(reply, "{\"ok\":true}\n");
        assert!(hub.pending_panes().contains(&"z".to_owned()));
    }

    #[test]
    fn the_configured_socket_follows_the_enabled_knob() {
        let on = FocusServerConfig {
            enabled: true,
            socket: Some("/x.sock".into()),
            ..FocusServerConfig::bare()
        };
        assert_eq!(configured_socket(&on), Some(PathBuf::from("/x.sock")));
        assert_eq!(configured_socket(&FocusServerConfig::bare()), None);
    }

    #[test]
    fn a_second_server_does_not_steal_a_live_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.sock");
        let policy = policy(0, FocusUnclaimed::Ignore);
        let _first = spawn_server(&path, FocusHub::new(policy)).unwrap();
        assert!(spawn_server(&path, FocusHub::new(policy)).is_err());
    }

    #[test]
    fn a_stale_socket_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert!(spawn_server(&path, FocusHub::new(policy(0, FocusUnclaimed::Ignore))).is_ok());
    }

    #[test]
    fn the_disabled_knob_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.sock");
        let cfg = FocusServerConfig {
            enabled: false,
            socket: Some(path.clone()),
            ..FocusServerConfig::bare()
        };
        let _relay = FocusRelay::start(&cfg);
        assert!(!path.exists());
    }

    #[test]
    fn the_relay_takes_its_policy_from_config() {
        let cfg = FocusServerConfig {
            claim_after_ms: 1234,
            unclaimed: FocusUnclaimed::Ignore,
            ..FocusServerConfig::bare()
        };
        let p = ClaimPolicy::from(&cfg);
        assert_eq!(p.grace, Duration::from_millis(1234));
        assert_eq!(p.unclaimed, FocusUnclaimed::Ignore);
        assert_eq!(
            ClaimPolicy::from(&FocusServerConfig::prescribed()),
            policy(300, FocusUnclaimed::SwitchLastFocused)
        );
    }

    #[test]
    fn drive_acknowledges_the_alert_of_the_window_that_serves_the_request() {
        use crate::pane_alert::Input;
        let hub = FocusHub::new(policy(0, FocusUnclaimed::Ignore));
        let win = hub.register(None);
        let cfg = crate::config::NotificationsConfig::prescribed();
        let dispatcher = tsuuchi::NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let mut center = NotificationCenter::new(dispatcher, &cfg);
        let t0 = Instant::now();
        center.pane_alert(t0, Input::Focus(false), None, false);
        assert!(center.pane_alert(t0, Input::Bell, None, false));
        assert!(center.has_pane_alert());
        hub.post(center.pane_group().to_owned());
        let wake = win.drive(
            state(center.pane_group().to_owned().as_str(), false, false),
            None,
            None,
            &mut center,
        );
        assert_eq!(wake, None);
        assert!(!center.has_pane_alert(), "focus clears the pane alert");
    }

    #[test]
    fn drive_leaves_another_windows_alert_alone() {
        use crate::pane_alert::Input;
        let hub = FocusHub::new(policy(0, FocusUnclaimed::Ignore));
        let a = hub.register(None);
        let b = hub.register(None);
        let cfg = crate::config::NotificationsConfig::prescribed();
        let mk = || {
            let d = tsuuchi::NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
            NotificationCenter::new(d, &cfg)
        };
        let (mut ca, mut cb) = (mk(), mk());
        ca.set_pane_group("pa");
        cb.set_pane_group("pb");
        let t0 = Instant::now();
        for c in [&mut ca, &mut cb] {
            c.pane_alert(t0, Input::Focus(false), None, false);
            assert!(c.pane_alert(t0, Input::Bell, None, false));
        }
        a.drive(state("pa", false, true), None, None, &mut ca);
        b.drive(state("pb", false, true), None, None, &mut cb);
        hub.post("pb".into());
        a.drive(state("pa", false, true), None, None, &mut ca);
        assert!(ca.has_pane_alert(), "A is not the window that shows pb");
        b.drive(state("pb", false, true), None, None, &mut cb);
        assert!(!cb.has_pane_alert());
    }

    fn click_over_the_wire(socket: &Path, card: &tsuuchi::Notification) -> String {
        let first = card.actions.first().expect("the card carries an action");
        let message = tsuuchi::ActionMessage::for_activation(&first.id, card.group.as_deref());
        let line = message.to_line().unwrap();
        let mut stream = UnixStream::connect(socket).unwrap();
        stream.write_all(line.as_bytes()).unwrap();
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply).unwrap();
        reply
    }

    #[test]
    fn a_real_card_click_over_the_real_socket_lands_on_the_window_that_shows_the_pane() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("focus.sock");
        let cfg = FocusServerConfig {
            enabled: true,
            socket: Some(socket.clone()),
            claim_after_ms: 50,
            unclaimed: FocusUnclaimed::SwitchLastFocused,
        };
        let relay = FocusRelay::start(&cfg);
        let a = relay.join(None);
        let b = relay.join(None);
        let (pa, pb) = (pane("window-a"), pane("window-b"));
        let t0 = Instant::now();
        a.service(state(&pa, false, true), t0);
        b.service(state(&pb, true, true), t0);

        let mut ncfg = crate::config::NotificationsConfig::prescribed();
        ncfg.pane_alerts.reasons.bell = crate::config::AlertDelivery::Notify;
        let d = tsuuchi::NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let mut center_b = NotificationCenter::new(d, &ncfg);
        center_b.set_pane_group(&pb);
        center_b.pane_alert(t0, crate::pane_alert::Input::Focus(false), None, false);
        center_b.pane_alert(t0, crate::pane_alert::Input::Bell, None, false);
        let card = center_b.recent(10).pop().expect("bell banner").notification;
        assert_eq!(card.group.as_deref(), Some(pb.as_str()));

        assert_eq!(click_over_the_wire(&socket, &card), "{\"ok\":true}\n");

        let now = Instant::now();
        let from_a = a.service(state(&pa, false, true), now);
        assert!(
            from_a.actions.is_empty(),
            "window A served B's click: {from_a:?}"
        );
        let from_b = b.service(state(&pb, true, true), now);
        assert_eq!(from_b.actions, vec![FocusAction::Raise]);
        assert!(relay.hub().pending_panes().is_empty());
    }

    #[test]
    fn a_real_click_for_a_pane_no_window_shows_switches_the_last_focused_window() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("focus.sock");
        let cfg = FocusServerConfig {
            enabled: true,
            socket: Some(socket.clone()),
            claim_after_ms: 50,
            unclaimed: FocusUnclaimed::SwitchLastFocused,
        };
        let relay = FocusRelay::start(&cfg);
        let a = relay.join(None);
        let b = relay.join(None);
        let (pa, pb) = (pane("window-a"), pane("window-b"));
        let gone = PaneId::from_seed("was-switched-away");
        let t0 = Instant::now();
        b.service(state(&pb, true, true), t0);
        a.service(state(&pa, false, true), t0);

        let card = tsuuchi::Notification::new("t", "b")
            .group(gone.to_string())
            .action(tsuuchi::NotificationAction::foreground(
                "focus_pane",
                "Focus",
            ));
        assert_eq!(click_over_the_wire(&socket, &card), "{\"ok\":true}\n");

        let now = Instant::now();
        assert!(a.service(state(&pa, false, true), now).actions.is_empty());
        assert!(b.service(state(&pb, false, true), now).actions.is_empty());
        std::thread::sleep(Duration::from_millis(80));
        let now = Instant::now();
        assert!(a.service(state(&pa, false, true), now).actions.is_empty());
        assert_eq!(
            b.service(state(&pb, false, true), now).actions,
            vec![FocusAction::Switch(gone), FocusAction::Raise]
        );
    }
}
