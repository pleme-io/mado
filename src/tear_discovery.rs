//! Tear-daemon discovery + auto-spawn for mado's launch path.
//!
//! Implements the contract documented in
//! [`MadoTearConfig`](crate::config::MadoTearConfig):
//!
//! ```text
//! Auto    → try connect; on failure, optionally auto_spawn; on
//!           still-failure, return None (caller falls back to local)
//! Always  → connect (auto_spawn if needed); error if still missing
//! Never   → return None unconditionally
//! Attach  → connect (no auto_spawn); error if missing
//! ```
//!
//! The auto-spawn path uses `Command::new("tear")` — relies on the
//! tear binary being on PATH (which it is on the pleme-io fleet via
//! the home-manager package). For non-pleme environments, set
//! `tear.mode = "never"` or explicitly point `tear.socket` at a
//! managed daemon.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tear_client::Client;

use crate::config::{MadoTearConfig, TearMode};

/// Outcome of a `discover` call. Lets the caller distinguish:
///   - `Attached`: a Client is connected; tear-attached mode wins.
///   - `Fallback`: tear was not available + config allowed
///     fall-through; mado should use the local PTY.
///   - `Required(_)`: tear was demanded but unavailable; mado
///     should propagate the error to the operator and refuse to
///     start.
///
/// `Required` is its own variant rather than a plain Err so the
/// caller can attach a helpful "what to try next" message at the
/// call site instead of guessing from a generic anyhow chain.
pub enum DiscoveryOutcome {
    Attached(Client, PathBuf),
    Fallback,
    Required(String),
}

impl DiscoveryOutcome {
    /// Short label for diagnostics + tests. Lets callers compare
    /// outcomes without matching past the inner data (which doesn't
    /// implement Debug because Client doesn't).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            DiscoveryOutcome::Attached(_, _) => "Attached",
            DiscoveryOutcome::Fallback => "Fallback",
            DiscoveryOutcome::Required(_) => "Required",
        }
    }
}

/// Resolve the socket path: explicit config override wins; else
/// fall back to the same default tear-daemon binds to (XDG runtime
/// dir → ~/.local/share/tear/tear.sock → /tmp/tear.sock).
pub fn resolve_socket_path(cfg: &MadoTearConfig) -> PathBuf {
    cfg.socket
        .clone()
        .unwrap_or_else(tear_types::wire::default_socket_path)
}

/// Top-level discovery entry. Honours every knob in
/// [`MadoTearConfig`] including the `Never` short-circuit.
pub fn discover(cfg: &MadoTearConfig) -> DiscoveryOutcome {
    if matches!(cfg.mode, TearMode::Never) {
        return DiscoveryOutcome::Fallback;
    }
    let socket_path = resolve_socket_path(cfg);

    // First attempt — try connecting to whatever's at the socket.
    if let Some(client) = try_connect(&socket_path) {
        tracing::info!(
            path = %socket_path.display(),
            "tear-daemon discovered + attached"
        );
        return DiscoveryOutcome::Attached(client, socket_path);
    }

    // Nothing answered. Maybe spawn one.
    let allow_spawn = match cfg.mode {
        TearMode::Auto | TearMode::Always => cfg.auto_spawn,
        TearMode::Attach => false,
        TearMode::Never => unreachable!("handled above"),
    };
    if allow_spawn {
        if let Err(e) = spawn_tear_daemon(&socket_path) {
            tracing::warn!(error = %e, "tear daemon spawn failed");
        } else {
            let deadline = Instant::now() + Duration::from_millis(cfg.spawn_wait_ms);
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
                if let Some(client) = try_connect(&socket_path) {
                    tracing::info!(
                        path = %socket_path.display(),
                        "tear-daemon auto-spawned + attached"
                    );
                    return DiscoveryOutcome::Attached(client, socket_path);
                }
            }
            tracing::warn!(
                path = %socket_path.display(),
                wait_ms = cfg.spawn_wait_ms,
                "auto-spawned tear daemon did not bind within deadline"
            );
        }
    }

    // Still nothing. Fall back if config allows; otherwise return
    // a Required error so the caller can refuse to launch.
    match cfg.mode {
        TearMode::Auto => DiscoveryOutcome::Fallback,
        TearMode::Always | TearMode::Attach => DiscoveryOutcome::Required(format!(
            "tear-daemon not reachable at {} (mode = {:?}, auto_spawn = {}).\n\
             Hint: `tear daemon` to start one, or set `tear.mode = \"auto\"` to allow fallback.",
            socket_path.display(),
            cfg.mode,
            cfg.auto_spawn
        )),
        TearMode::Never => unreachable!("handled above"),
    }
}

/// One probe attempt. Returns Some(Client) on success, None on any
/// failure (NotFound, ConnectionRefused, PermissionDenied, etc.).
/// Quiet because a missing daemon is a normal Auto/Never case.
pub fn try_connect(socket_path: &Path) -> Option<Client> {
    Client::connect(socket_path).ok()
}

/// Spawn `tear daemon --socket <path>` and detach. Best-effort —
/// failure here is non-fatal; the caller decides whether to
/// fallback or surface the error based on `MadoTearConfig::mode`.
fn spawn_tear_daemon(socket_path: &Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let path_str = socket_path.to_string_lossy().to_string();
    let child = Command::new("tear")
        .args(["daemon", "--socket"])
        .arg(&path_str)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    tracing::info!(
        pid = child.id(),
        path = %socket_path.display(),
        "tear daemon auto-spawned"
    );
    // Don't wait — we want the daemon to live independently of mado.
    // The DiscoveryOutcome::Attached client holds the connection;
    // when mado exits, the daemon outlives it (the intended shape
    // — sessions survive mado restarts).
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(mode: TearMode, socket: &Path, auto_spawn: bool) -> MadoTearConfig {
        MadoTearConfig {
            mode,
            socket: Some(socket.to_path_buf()),
            auto_spawn,
            spawn_wait_ms: 100,
            session_name: None,
            pane: None,
            impose: None,
        }
    }

    #[test]
    fn never_mode_short_circuits_to_fallback() {
        let cfg = cfg_with(TearMode::Never, Path::new("/nonexistent.sock"), true);
        // Must not even try to connect or spawn — fallback immediately.
        assert!(matches!(discover(&cfg), DiscoveryOutcome::Fallback));
    }

    #[test]
    fn auto_mode_falls_back_when_no_daemon_and_no_spawn() {
        let cfg = cfg_with(
            TearMode::Auto,
            Path::new("/tmp/no-such-tear-socket-9181.sock"),
            false, // explicitly disable auto_spawn
        );
        assert!(matches!(discover(&cfg), DiscoveryOutcome::Fallback));
    }

    #[test]
    fn attach_mode_returns_required_when_no_daemon() {
        let cfg = cfg_with(
            TearMode::Attach,
            Path::new("/tmp/no-such-tear-socket-9182.sock"),
            true, // even with auto_spawn, Attach refuses to spawn
        );
        match discover(&cfg) {
            DiscoveryOutcome::Required(msg) => {
                assert!(msg.contains("not reachable"));
                assert!(msg.contains("Attach"));
            }
            other => panic!("expected Required, got {}", other.kind()),
        }
    }

    #[test]
    fn always_mode_returns_required_when_spawn_disabled_and_no_daemon() {
        let cfg = cfg_with(
            TearMode::Always,
            Path::new("/tmp/no-such-tear-socket-9183.sock"),
            false,
        );
        assert!(matches!(discover(&cfg), DiscoveryOutcome::Required(_)));
    }

    #[test]
    fn discovers_a_real_in_process_daemon() {
        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("mado-discovery-test-{pid}.sock"));
            p
        };
        let inproc = std::sync::Arc::new(tear_core::InProcess::new());
        let daemon = tear_daemon::start(socket.clone(), inproc).expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let cfg = cfg_with(TearMode::Auto, &socket, false);
        match discover(&cfg) {
            DiscoveryOutcome::Attached(_client, path) => {
                assert_eq!(path, socket);
            }
            DiscoveryOutcome::Fallback => panic!("should have discovered live daemon"),
            DiscoveryOutcome::Required(_) => panic!("Auto shouldn't return Required"),
        }
        daemon.stop();
    }

    #[test]
    fn resolve_socket_path_honours_explicit_override() {
        let explicit = std::path::PathBuf::from("/tmp/operator-managed.sock");
        let cfg = MadoTearConfig {
            socket: Some(explicit.clone()),
            ..MadoTearConfig::default()
        };
        assert_eq!(resolve_socket_path(&cfg), explicit);
    }

    #[test]
    fn resolve_socket_path_falls_back_to_xdg_default_when_unset() {
        let cfg = MadoTearConfig {
            socket: None,
            ..MadoTearConfig::default()
        };
        // The exact path depends on env, but it should always end in
        // "tear.sock" — same default the tear daemon binds to.
        let p = resolve_socket_path(&cfg);
        assert!(p.to_string_lossy().ends_with("tear.sock"));
    }

    /// Full impose round-trip: discovery + get_config + apply_to +
    /// set_config + verify daemon-side LiveConfig reflects the
    /// override. Equivalent to what gui_tear_attach / cmd_tear_attach
    /// do at attach time — exercised here in isolation.
    #[test]
    fn impose_flow_overrides_daemon_config_via_discovery_then_set_config() {
        use crate::config::MadoTearImpose;
        use tear_types::MultiplexerControl;

        let socket = {
            let mut p = std::env::temp_dir();
            let pid = std::process::id();
            p.push(format!("mado-impose-flow-{pid}.sock"));
            p
        };
        let inproc = std::sync::Arc::new(tear_core::InProcess::new());
        let live = std::sync::Arc::new(tear_config::LiveConfig::default());
        let daemon =
            tear_daemon::start_with_config(socket.clone(), inproc, live.clone()).expect("daemon");
        std::thread::sleep(std::time::Duration::from_millis(50));

        let cfg = MadoTearConfig {
            mode: TearMode::Auto,
            socket: Some(socket.clone()),
            auto_spawn: false,
            spawn_wait_ms: 100,
            session_name: None,
            pane: None,
            impose: Some(MadoTearImpose {
                prefix: Some("C-x".into()),
                default_shell: Some("/bin/fish".into()),
                status_visible: Some(false),
            }),
        };

        // 1 — discover the daemon
        let client = match discover(&cfg) {
            DiscoveryOutcome::Attached(c, _) => c,
            other => panic!("expected Attached, got {}", other.kind()),
        };

        // 2 — emulate the same impose flow gui_tear_attach runs at
        //     attach time: get → apply → set.
        let mut current = client.get_config().expect("get_config");
        cfg.impose.as_ref().unwrap().apply_to(&mut current);
        client.set_config(&current).expect("set_config");

        // 3 — daemon-side LiveConfig reflects the impose. Read
        //     directly via the shared Arc rather than another RPC
        //     round-trip so we exercise the live state, not a
        //     re-fetched snapshot.
        let after = live.load();
        assert_eq!(after.prefix, "C-x");
        assert_eq!(after.default_shell, "/bin/fish");
        assert!(!after.status.visible);

        // 4 — dynamic re-impose during the session (the use case
        //     the user explicitly called out — mado may want to
        //     change tear's config later, not just at attach).
        let mut next = client.get_config().expect("get_config");
        let dyn_impose = MadoTearImpose {
            prefix: Some("C-Space".into()),
            ..MadoTearImpose::default()
        };
        dyn_impose.apply_to(&mut next);
        client.set_config(&next).expect("set_config 2");
        let after_dyn = live.load();
        assert_eq!(after_dyn.prefix, "C-Space");
        // default_shell unchanged from previous impose (None means
        // "leave alone", not "reset").
        assert_eq!(after_dyn.default_shell, "/bin/fish");

        daemon.stop();
    }
}
