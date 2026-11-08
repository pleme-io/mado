//! The `mado ibuki` endpoint, live: this binary as the endpoint, a real
//! UDP socket, blue's own `Wire` as the client. Pins the binary protocol
//! end to end — the version facts, a typed refusal for an unknown op, one
//! real MCP tool relay (`status` — headless-safe, no PTY), and a bulk
//! probe answered over the Tide with the acks feeding the controller.

use std::net::ToSocketAddrs;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use blue_lang_runtime::wire::{Channel, Payloads, RING_BYTES, RING_FRAMES, Wire};

/// The endpoint child, killed and reaped when the test ends however it
/// ends: a panicking assertion must not leave an endpoint holding the
/// test's stdout pipe open, which reads as a test that never finishes.
struct Endpoint(Child);

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_endpoint() -> (Endpoint, std::net::SocketAddr) {
    let bin = env!("CARGO_BIN_EXE_mado");
    let mut child = Command::new(bin)
        .args(["ibuki", "--bind", "127.0.0.1:0"])
        .env("RUST_LOG", "error")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn `mado ibuki`");
    let stdout = child.stdout.take().expect("piped stdout");
    let child = Endpoint(child);
    // The endpoint prints exactly one line: `ibuki listening on <addr>`.
    use std::io::BufRead;
    let reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    for l in reader.lines() {
        let l = l.expect("read endpoint stdout");
        if let Some(rest) = l.strip_prefix("ibuki listening on ") {
            line = rest.to_owned();
            break;
        }
    }
    let addr = line
        .to_socket_addrs()
        .expect("the printed address resolves")
        .next()
        .expect("one address");
    (child, addr)
}

fn client() -> Wire {
    let mut w = Wire::bind("127.0.0.1:0").expect("client binds");
    w.keep(Payloads::Bytes);
    w.open(Channel::Flash, (RING_FRAMES, RING_BYTES));
    w.open(Channel::Tide, (RING_FRAMES, RING_BYTES));
    w
}

fn recv_flash(w: &mut Wire) -> blue_lang_runtime::wire::Received {
    w.next(Channel::Flash, Duration::from_secs(20))
        .expect("flash recv")
        .expect("a flash reply")
}

#[test]
fn the_endpoint_answers_the_binary_protocol_over_udp() {
    let (endpoint, addr) = spawn_endpoint();
    let mut w = client();

    // version: [01] → [81][protocol u8][version str16]
    w.flash_put(addr, &[0x01], 0).expect("send version");
    let reply = recv_flash(&mut w);
    let bytes = reply.raw.as_slice();
    assert_eq!(bytes[0], 0x81, "ok-small");
    assert_eq!(bytes[1], 1, "protocol version 1");
    let vlen = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    let version = std::str::from_utf8(&bytes[4..4 + vlen]).expect("utf8 version");
    assert_eq!(version, env!("CARGO_PKG_VERSION"));

    // an unknown op byte is a typed refusal, never silence: [09] →
    // [83][kind=1][why str16]
    w.flash_put(addr, &[0x09], 0).expect("send unknown op");
    let reply = recv_flash(&mut w);
    let bytes = reply.raw.as_slice();
    assert_eq!(bytes[0], 0x83);
    assert_eq!(bytes[1], 1, "refused kind = frame");
    assert!(bytes.len() > 4);

    // one real MCP tool relay: `status` — small result on Flash.
    // call: [02][tool len u8][tool][args len u16][args]
    let mut call = vec![0x02, 6];
    call.extend_from_slice(b"status");
    call.extend_from_slice(&0u16.to_be_bytes());
    w.flash_put(addr, &call, 0).expect("send call status");
    let reply = recv_flash(&mut w);
    let bytes = reply.raw.as_slice();
    assert_eq!(bytes[0], 0x81, "status is a small reply");
    let len = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
    let body = std::str::from_utf8(&bytes[3..3 + len]).expect("utf8 body");
    assert!(body.contains("version"), "status body: {body}");
    drop(endpoint);
}

#[test]
fn a_bulk_probe_rides_the_tide_and_acks_feed_the_controller() {
    let (endpoint, addr) = spawn_endpoint();
    let mut w = client();

    // probe 8 KiB: [03][bytes u32]
    let want = 8 * 1024usize;
    let mut probe = vec![0x03];
    probe.extend_from_slice(&(want as u32).to_be_bytes());
    w.flash_put(addr, &probe, 0).expect("send probe");

    // The Flash notice: [82][frames u16][queued_frames u16][queued_bytes
    // u32][cwnd u32].
    let reply = recv_flash(&mut w);
    let bytes = reply.raw.as_slice();
    assert_eq!(bytes[0], 0x82, "ok-tide");
    let frames = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
    let cwnd0 = u32::from_be_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]);
    assert!(frames >= want / 1271, "{frames} frames for {want} bytes");
    assert_eq!(cwnd0, 2560, "the window starts at its floor");

    // Read Tide frames as they are released, ack each: [04][bytes u32]
    // [delay_ms i32]. The first release (one window's worth) is already
    // out; the rest needs the pump, fed by our acks.
    let mut got = 0usize;
    let mut acked_bytes = 0u64;
    let deadline = Instant::now() + Duration::from_secs(30);
    while got < want {
        assert!(
            Instant::now() < deadline,
            "the Tide stopped after {got} of {want} bytes"
        );
        if let Ok(Some(frame)) = w.next(Channel::Tide, Duration::from_secs(1)) {
            got += frame.raw.len();
            let delay = 1i64; // a steady, believable one-way delay
            acked_bytes += frame.raw.len() as u64;
            let mut ack = vec![0x04];
            ack.extend_from_slice(&(frame.raw.len() as u32).to_be_bytes());
            ack.extend_from_slice(&(delay as i32).to_be_bytes());
            w.flash_put(addr, &ack, 0).expect("send ack");
        }
    }
    assert_eq!(got, want);
    assert!(acked_bytes >= want as u64);
    drop(endpoint);
}
