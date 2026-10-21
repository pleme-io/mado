use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::FocusServerConfig;
use crate::notify_center::NotificationCenter;
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

#[derive(Debug, Clone, Default)]
pub struct FocusRequests {
    pending: Arc<Mutex<Option<String>>>,
}

impl FocusRequests {
    pub fn post(&self, pane: String) {
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pane);
        crate::ring::WINDOW.ring();
    }

    #[must_use]
    pub fn take(&self) -> Option<String> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

pub fn handle_line(line: &str, requests: &FocusRequests) -> String {
    match parse_line(line) {
        Ok(pane) => {
            requests.post(pane);
            "{\"ok\":true}\n".to_owned()
        }
        Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}\n", e.code()),
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

fn serve_connection(stream: UnixStream, requests: &FocusRequests) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut line = String::new();
    let read = BufReader::new((&stream).take(MAX_LINE_BYTES)).read_line(&mut line);
    let reply = match read {
        Ok(0) | Err(_) => return,
        Ok(_) => handle_line(&line, requests),
    };
    let _ = (&stream).write_all(reply.as_bytes());
}

pub fn spawn_server(path: &Path, requests: FocusRequests) -> std::io::Result<FocusServer> {
    let listener = bind(path)?;
    let spawned = std::thread::Builder::new()
        .name("mado-focus".to_owned())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                serve_connection(stream, &requests);
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

pub struct FocusRelay {
    requests: FocusRequests,
    _server: Option<FocusServer>,
}

impl FocusRelay {
    #[must_use]
    pub fn start(cfg: &FocusServerConfig) -> Self {
        let requests = FocusRequests::default();
        let server = configured_socket(cfg).map(|path| {
            match spawn_server(&path, requests.clone()) {
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
            requests,
            _server: server.flatten(),
        }
    }

    pub fn service(&self, switch: Option<&SwitchRequests>, notify: &mut NotificationCenter) {
        let Some(requested) = self.requests.take() else {
            return;
        };
        let can_switch = switch.is_some_and(SwitchRequests::sink_attached);
        match plan(&requested, notify.pane_group(), can_switch) {
            FocusPlan::Raise => {}
            FocusPlan::SwitchThenRaise(pane) => {
                if let Some(sw) = switch {
                    sw.post(pane);
                }
            }
            FocusPlan::Reject(reason) => {
                tracing::warn!(pane = %requested, reason, "focus request not served");
                return;
            }
        }
        crate::platform::raise_app();
        notify.acknowledge(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

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
    fn a_valid_line_is_queued_and_acknowledged_last_write_wins() {
        let r = FocusRequests::default();
        assert_eq!(handle_line("{\"focus_pane\":\"a\"}", &r), "{\"ok\":true}\n");
        assert_eq!(handle_line("{\"focus_pane\":\"b\"}", &r), "{\"ok\":true}\n");
        assert_eq!(r.take(), Some("b".into()));
        assert_eq!(r.take(), None);
    }

    #[test]
    fn a_refused_line_queues_nothing() {
        let r = FocusRequests::default();
        assert_eq!(
            handle_line("{\"focus_pane\":\"\"}", &r),
            "{\"ok\":false,\"error\":\"empty-pane\"}\n"
        );
        assert_eq!(r.take(), None);
    }

    #[test]
    fn the_displayed_pane_is_raised_without_a_switch() {
        assert_eq!(plan("local", "local", false), FocusPlan::Raise);
    }

    #[test]
    fn another_pane_switches_only_when_a_switch_sink_exists() {
        let other = tear_types::PaneId::from_seed("other");
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
    fn a_client_that_closes_before_the_reply_does_not_stop_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.sock");
        let requests = FocusRequests::default();
        let _server = spawn_server(&path, requests.clone()).unwrap();
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
        assert_eq!(requests.take().as_deref(), Some("z"));
    }

    #[test]
    fn the_configured_socket_follows_the_enabled_knob() {
        let on = FocusServerConfig {
            enabled: true,
            socket: Some("/x.sock".into()),
        };
        assert_eq!(configured_socket(&on), Some(PathBuf::from("/x.sock")));
        assert_eq!(configured_socket(&FocusServerConfig::bare()), None);
    }

    #[test]
    fn the_socket_serves_one_line_and_replies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.sock");
        let requests = FocusRequests::default();
        let server = spawn_server(&path, requests.clone()).unwrap();
        let mut c = UnixStream::connect(&path).unwrap();
        c.write_all(b"{\"focus_pane\":\"pane-7\"}\n").unwrap();
        let mut reply = String::new();
        BufReader::new(&c).read_line(&mut reply).unwrap();
        assert_eq!(reply, "{\"ok\":true}\n");
        assert_eq!(requests.take(), Some("pane-7".into()));
        drop(server);
        assert!(!path.exists());
    }

    #[test]
    fn a_second_server_does_not_steal_a_live_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.sock");
        let _first = spawn_server(&path, FocusRequests::default()).unwrap();
        assert!(spawn_server(&path, FocusRequests::default()).is_err());
    }

    #[test]
    fn a_stale_socket_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert!(spawn_server(&path, FocusRequests::default()).is_ok());
    }

    #[test]
    fn the_disabled_knob_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focus.sock");
        let cfg = FocusServerConfig {
            enabled: false,
            socket: Some(path.clone()),
        };
        let _relay = FocusRelay::start(&cfg);
        assert!(!path.exists());
    }

    #[test]
    fn service_acknowledges_the_alert_of_the_displayed_pane() {
        use crate::pane_alert::Input;
        let relay = FocusRelay {
            requests: FocusRequests::default(),
            _server: None,
        };
        let cfg = crate::config::NotificationsConfig::prescribed();
        let dispatcher = tsuuchi::NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()));
        let mut center = NotificationCenter::new(dispatcher, &cfg);
        let t0 = Instant::now();
        center.pane_alert(t0, Input::Focus(false), None, false);
        assert!(center.pane_alert(t0, Input::Bell, None, false));
        assert!(center.has_pane_alert());
        relay.requests.post(center.pane_group().to_owned());
        relay.service(None, &mut center);
        assert!(!center.has_pane_alert(), "focus clears the pane alert");
    }
}
