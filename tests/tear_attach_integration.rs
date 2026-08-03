//! Phase-3 MVP integration test: `mado tear-attach` end-to-end.
//!
//! Choreography:
//!   1. Spawn `tear-daemon` on a private UDS in this process via
//!      `tear_daemon::start` (no subprocess overhead, no flaky
//!      "did the daemon bind yet" sleep).
//!   2. Create a `/bin/sh` session in the daemon via `tear-client`.
//!   3. Spawn `mado tear-attach <pane> --socket <sock>` as a
//!      subprocess with a piped stdout/stdin.
//!   4. Send a known `printf MARKER\n` to the pane via tear-client.
//!   5. Read mado's stdout, assert the marker text appears.
//!   6. Kill mado + drop the daemon handle.
//!
//! Proves the full Phase-3-MVP vertical:
//!   tear-client → daemon → InProcess PTY → child shell → reader
//!   thread → on_bytes → subscribers → daemon subscription serve
//!   thread → CBOR over UDS → mado-as-subscriber → mado's stdout.
//!
//! The mado binary used is whatever `cargo test` built for the
//! current package — `env!("CARGO_BIN_EXE_mado")` resolves to the
//! freshly-built binary path. No assumptions about $PATH.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn mado_tear_attach_streams_pane_output_to_stdout() {
    // Per-test socket to allow parallel runs.
    let socket = {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        p.push(format!("mado-tear-attach-test-{pid}.sock"));
        p
    };
    let _ = std::fs::remove_file(&socket);

    // 1 — daemon
    let inproc = Arc::new(tear_core::InProcess::new());
    let daemon = tear_daemon::start(socket.clone(), inproc).expect("tear-daemon should start");
    std::thread::sleep(Duration::from_millis(50));

    // 2 — control client + session
    use tear_types::MultiplexerControl;
    let ctrl = tear_client::Client::connect(&socket).expect("connect");
    let sid = ctrl
        .new_session("mado-attach-test", "/bin/sh")
        .expect("new_session");
    let session = ctrl.get_session(sid).expect("get_session");
    let pane_id = *session.panes.keys().next().expect("pane");

    // 3 — mado subprocess
    let mado_bin = env!("CARGO_BIN_EXE_mado");
    let mut child = Command::new(mado_bin)
        .args(["tear-attach", &pane_id.to_string(), "--socket"])
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("mado tear-attach should spawn");

    // Give mado a beat to dial the daemon + register its subscriber.
    std::thread::sleep(Duration::from_millis(300));

    // 4 — send a known marker
    let marker = "MADO_TEAR_INTEGRATION_MARK_71309";
    let cmd = format!("printf '{marker}\\n'\n");
    ctrl.send_keys(pane_id, cmd.as_bytes()).expect("send_keys");

    // 5 — poll mado's stdout for the marker
    let mut stdout = child.stdout.take().expect("stdout pipe");
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut accum = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        // Best-effort non-blocking-ish read — block briefly then
        // poll the buffer for the marker. The OS pipe has data
        // when the daemon pushes Bytes to mado and mado writes to
        // stdout.
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                accum.extend_from_slice(&buf[..n]);
                if std::str::from_utf8(&accum)
                    .map(|s| s.contains(marker))
                    .unwrap_or(false)
                {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("read mado stdout: {e}"),
        }
    }

    // 6 — clean up before assertions so a failure doesn't leak the
    //     subprocess. Kill is best-effort; SIGTERM-equivalent.
    let _ = child.kill();
    let _ = child.wait();
    daemon.stop();

    let got = String::from_utf8_lossy(&accum).to_string();
    assert!(
        got.contains(marker),
        "mado tear-attach did not stream marker `{marker}`.\n\
         Accumulated bytes ({} total):\n{got}",
        accum.len()
    );
}
