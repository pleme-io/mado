//! `mado ibuki` — the wire endpoint a docked blue program drives.
//!
//! A blue program granted `comm_flash` (and `comm_scavenger` for bulk)
//! can open this endpoint over UDP and drive mado's whole typed MCP
//! surface — the same 63 tools an agent reaches over stdio — through
//! blue's own wire transport (`blue_lang_runtime::wire::Wire`: the
//! framing, the MTU and the Flash bucket are blue's, not a copy). This
//! is the docking surface: a docked app with a
//! terminal face runs in a mado session.
//!
//! ## The protocol (version 1, BINARY)
//!
//! Every frame is a compact binary message — the envelope is never
//! text. The op and status discriminants are authored byte tables
//! ([`Op`]/[`Status`] via the macro-farm's `KindByte` paired round-trip
//! derive), integers are fixed-width big-endian, strings are
//! length-prefixed UTF-8. The hot path matters most: a Tide ack is 9
//! bytes parsed at fixed offsets, not ~40 bytes of JSON through a
//! parser — one ack arrives per bulk frame, so this is the difference
//! between the pacer and its own bottleneck. MCP tool ARGUMENTS and
//! RESULTS are the one payload kept as opaque JSON: they cross the
//! rmcp boundary as JSON regardless, so re-encoding them would buy
//! nothing and cost a schema.
//!
//! Requests arrive as Flash frames. Small replies leave as Flash
//! frames; a reply too large for one frame rides the TIDE, paced by
//! the LEDBAT window (RFC 6817, mirrored in `mado::ibuki::Tide` from
//! blue's bidama): the client measures each Tide frame's one-way delay
//! against its stamp and answers an `ack` on Flash; the endpoint's
//! controller widens and narrows the window, and releases only what
//! the window allows.
//!
//! v1 holds ONE Tide controller for the endpoint (one docked peer at a
//! time is the shape of the thing it serves); per-peer controllers are
//! the named follow-up.

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use blue_lang_runtime::wire::{Channel, Payloads, RING_BYTES, RING_FRAMES, Wire};
use pleme_kindbyte_derive::KindByte;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

use mado::ibuki::{Ack, Tide};

/// The endpoint's protocol version, carried by the `version` reply.
const PROTOCOL: u8 = 1;

/// A request discriminant. The byte table is the protocol — a new op
/// is a new authored byte, and the derive keeps encode/decode honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, KindByte)]
pub enum Op {
    /// Ask the endpoint's facts: protocol version + mado version.
    #[kind(byte = 0x01)]
    Version,
    /// Relay one MCP tool: `[tool: str8][args-json: str16]`.
    #[kind(byte = 0x02)]
    Call,
    /// Send N bytes back over the Tide: `[bytes: u32]`. The wire's
    /// self-test and the bulk path's exercise.
    #[kind(byte = 0x03)]
    Probe,
    /// Tide feedback: `[bytes: u32][delay_ms: i32]`.
    #[kind(byte = 0x04)]
    Ack,
}

/// A reply discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, KindByte)]
pub enum Status {
    /// Ok, small: the payload IS the answer: `[body: str16]`.
    #[kind(byte = 0x81)]
    OkSmall,
    /// Ok, bulk queued on the Tide:
    /// `[frames: u16][queued_frames: u16][queued_bytes: u32][cwnd: u32]`.
    #[kind(byte = 0x82)]
    OkTide,
    /// A typed refusal: `[kind: u8][why: str16]`.
    #[kind(byte = 0x83)]
    Refused,
}

/// A parsed request.
#[derive(Debug, PartialEq)]
enum Request {
    Version,
    Call { tool: String, args: Value },
    Probe { bytes: u32 },
    Ack { bytes: u64, delay_ms: i64 },
}

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefusedKind {
    Frame = 1,
    Tool = 2,
    Ring = 3,
}

// ── the codec: fixed-width BE integers, length-prefixed UTF-8 ────────

fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_str16(out: &mut Vec<u8>, s: &str) {
    let len = u16::try_from(s.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn take_u32(at: &mut usize, buf: &[u8]) -> Option<u32> {
    let end = at.checked_add(4)?;
    let slice = buf.get(*at..end)?;
    *at = end;
    Some(u32::from_be_bytes(slice.try_into().ok()?))
}

fn take_i32(at: &mut usize, buf: &[u8]) -> Option<i32> {
    take_u32(at, buf).map(|n| n as i32)
}

fn take_str(at: &mut usize, buf: &[u8], len_bytes: usize) -> Option<String> {
    let len_end = at.checked_add(len_bytes)?;
    let len_slice = buf.get(*at..len_end)?;
    let mut len = 0usize;
    for b in len_slice {
        len = (len << 8) | usize::from(*b);
    }
    let end = len_end.checked_add(len)?;
    let s = std::str::from_utf8(buf.get(len_end..end)?).ok()?;
    *at = end;
    Some(s.to_owned())
}

/// Decode one request. `Ok(None)` = an unknown op byte: the caller
/// answers a typed refusal naming the byte.
fn decode(buf: &[u8]) -> Result<Option<Request>, String> {
    let (&first, rest) = buf.split_first().ok_or("empty frame")?;
    let Some(op) = Op::from_byte(first) else {
        return Ok(None);
    };
    let mut at = 0usize;
    match op {
        Op::Version if rest.is_empty() => Ok(Some(Request::Version)),
        Op::Version => Err("version carries no body".into()),
        Op::Call => {
            let tool = take_str(&mut at, rest, 1).ok_or("call needs [tool: str8][args: str16]")?;
            let args_json =
                take_str(&mut at, rest, 2).ok_or("call needs [tool: str8][args: str16]")?;
            let args: Value = if args_json.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&args_json).map_err(|e| format!("args: {e}"))?
            };
            Ok(Some(Request::Call { tool, args }))
        }
        Op::Probe => {
            let bytes = take_u32(&mut at, rest).ok_or("probe needs [bytes: u32]")?;
            Ok(Some(Request::Probe { bytes }))
        }
        Op::Ack => {
            let bytes = take_u32(&mut at, rest).ok_or("ack needs [bytes: u32][delay_ms: i32]")?;
            let delay_ms =
                take_i32(&mut at, rest).ok_or("ack needs [bytes: u32][delay_ms: i32]")?;
            Ok(Some(Request::Ack {
                bytes: u64::from(bytes),
                delay_ms: i64::from(delay_ms),
            }))
        }
    }
}

/// Encode a typed refusal.
fn refused(kind: RefusedKind, why: &str) -> Vec<u8> {
    let mut out = vec![Status::Refused.as_byte(), kind as u8];
    put_str16(&mut out, why);
    out
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
    )
    .unwrap_or(0)
}

type Client = RunningService<RoleClient, ()>;

async fn spawn_client() -> anyhow::Result<Client> {
    let mado_bin = std::env::current_exe().context("resolve current mado binary path")?;
    let mut cmd = tokio::process::Command::new(&mado_bin);
    cmd.arg("mcp").env("RUST_LOG", "error").kill_on_drop(true);
    let transport = TokioChildProcess::new(cmd).context("spawn `mado mcp` child")?;
    ().serve(transport)
        .await
        .context("rmcp client handshake with `mado mcp` child")
}

/// Run the endpoint until killed. Prints the bound address as one
/// `ibuki listening on <addr>` line so a supervisor (or a test) can
/// find the dock without a port file.
pub fn serve(bind: &str) -> anyhow::Result<()> {
    shidou::init_tracing_to_stderr();
    let rt = shidou::create_runtime()?;
    let client = rt.block_on(spawn_client())?;

    let mut wire = Wire::bind(bind).context("bind the ibuki wire endpoint")?;
    wire.keep(Payloads::Bytes);
    wire.open(Channel::Flash, (RING_FRAMES, RING_BYTES));
    wire.open(Channel::Tide, (RING_FRAMES, RING_BYTES));
    let address = wire.address().context("resolve the bound address")?;
    println!("ibuki listening on {address}");

    let mut tide = Tide::new();
    let mut last_pump = Instant::now();

    loop {
        if let Ok(Some(received)) = wire.next(Channel::Flash, Duration::from_millis(50)) {
            let from = received.from;
            let request = match decode(&received.raw) {
                Ok(Some(request)) => request,
                Ok(None) => {
                    let byte = received.raw.first().copied().unwrap_or(0);
                    flash_bytes(
                        &mut wire,
                        from,
                        &refused(
                            RefusedKind::Frame,
                            &format!("op byte {byte:#04x} names no op"),
                        ),
                    );
                    continue;
                }
                Err(why) => {
                    flash_bytes(&mut wire, from, &refused(RefusedKind::Frame, &why));
                    continue;
                }
            };
            match request {
                Request::Version => {
                    let mut out = vec![Status::OkSmall.as_byte(), PROTOCOL];
                    put_str16(&mut out, env!("CARGO_PKG_VERSION"));
                    flash_bytes(&mut wire, from, &out);
                }
                Request::Call { tool, args } => {
                    let call = async {
                        let params = CallToolRequestParams {
                            name: tool.clone().into(),
                            arguments: args.as_object().cloned(),
                            meta: None,
                            task: None,
                        };
                        let result = client
                            .call_tool(params)
                            .await
                            .with_context(|| format!("call_tool({tool})"))?;
                        let text = result
                            .content
                            .into_iter()
                            .find_map(|c| match c.raw {
                                rmcp::model::RawContent::Text(t) => Some(t.text),
                                _ => None,
                            })
                            .unwrap_or_default();
                        anyhow::Ok(text)
                    };
                    match rt.block_on(call) {
                        Ok(text) => bulk(&mut wire, from, &mut tide, text),
                        Err(e) => {
                            flash_bytes(
                                &mut wire,
                                from,
                                &refused(RefusedKind::Tool, &format!("{e:#}")),
                            );
                        }
                    }
                }
                Request::Probe { bytes } => {
                    let body = "x".repeat(usize::try_from(bytes.min(64 * 1024)).unwrap_or(0));
                    bulk(&mut wire, from, &mut tide, body);
                }
                Request::Ack { bytes, delay_ms } => {
                    tide = tide.ack(Ack {
                        bytes,
                        delay_ms,
                        flight: None,
                        now_ms: now_ms(),
                    });
                }
            }
        }
        // The Tide pump: release what the window allows, on every wake
        // (a 50 ms cadence while quiet, immediately after every op).
        if last_pump.elapsed() >= Duration::from_millis(50) {
            last_pump = Instant::now();
            let window = tide.cwnd().max(0) as usize;
            if wire.queued().0 > 0 && window > 0 {
                let _ = wire.tide_release(window, now_ms());
            }
        }
    }
}

/// Send one small binary reply through the Flash bucket; a frame a
/// busy bucket refuses is retried once after its wait, exactly as the
/// worker border does.
fn flash_bytes(wire: &mut Wire, peer: SocketAddr, body: &[u8]) {
    let mut now = now_ms();
    if let Err(first) = wire.flash_put(peer, body, now) {
        if let blue_lang_runtime::wire::SendError::Rate(refusal) = &first {
            if let Some(wait) = refusal.wait_ms {
                std::thread::sleep(Duration::from_millis(u64::try_from(wait).unwrap_or(0)));
                now += i64::try_from(wait).unwrap_or(i64::MAX);
                let _ = wire.flash_put(peer, body, now);
            }
        }
    }
}

/// The bytes an `OkSmall` reply spends before its body: the status byte
/// and the body's u16 length.
const OK_SMALL_HEADER: usize = 3;

/// Whether a body travels as one `OkSmall` Flash frame: the header and
/// the body together within the frame's room.
fn fits_one_flash(body: usize, room: usize) -> bool {
    body.saturating_add(OK_SMALL_HEADER) <= room
}

/// Send `body` to `peer`: one Flash frame when it fits, else Tide
/// frames announced by a Flash notice.
fn bulk(wire: &mut Wire, peer: SocketAddr, tide: &mut Tide, body: String) {
    if fits_one_flash(body.len(), Channel::Flash.room()) {
        let mut out = vec![Status::OkSmall.as_byte()];
        put_str16(&mut out, &body);
        flash_bytes(wire, peer, &out);
        return;
    }
    let chunks: Vec<&[u8]> = body.as_bytes().chunks(Channel::Tide.room()).collect();
    for chunk in &chunks {
        // One payload per frame; tide_put encodes and queues it.
        if wire.tide_put(peer, chunk).is_err() {
            let why = "the Tide send ring is full; ack what is in flight";
            flash_bytes(wire, peer, &refused(RefusedKind::Ring, why));
            return;
        }
    }
    let queued = wire.queued();
    let mut out = vec![Status::OkTide.as_byte()];
    out.extend_from_slice(&(chunks.len() as u16).to_be_bytes());
    out.extend_from_slice(&(queued.0 as u16).to_be_bytes());
    put_u32(&mut out, u32::try_from(queued.1).unwrap_or(u32::MAX));
    put_u32(&mut out, tide.cwnd().max(0) as u32);
    flash_bytes(wire, peer, &out);
    let window = tide.cwnd().max(0) as usize;
    if window > 0 {
        let _ = wire.tide_release(window, now_ms());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_op_and_status_bytes_are_the_authored_table() {
        assert_eq!(Op::Version.as_byte(), 0x01);
        assert_eq!(Op::Call.as_byte(), 0x02);
        assert_eq!(Op::Probe.as_byte(), 0x03);
        assert_eq!(Op::Ack.as_byte(), 0x04);
        assert_eq!(Status::OkSmall.as_byte(), 0x81);
        assert_eq!(Status::OkTide.as_byte(), 0x82);
        assert_eq!(Status::Refused.as_byte(), 0x83);
        for op in [Op::Version, Op::Call, Op::Probe, Op::Ack] {
            assert_eq!(Op::from_byte(op.as_byte()), Some(op));
        }
    }

    #[test]
    fn every_request_round_trips_and_the_bytes_are_pinned() {
        // version: the op byte alone.
        assert_eq!(decode(&[0x01]).unwrap(), Some(Request::Version));
        // call: [op][tool len u8][tool][args len u16][args]
        let args = r#"{"rows":5}"#;
        let mut call = vec![0x02, 3, b'v', b'e', b'r'];
        call.extend_from_slice(&(args.len() as u16).to_be_bytes());
        call.extend_from_slice(args.as_bytes());
        match decode(&call).unwrap() {
            Some(Request::Call { tool, args }) => {
                assert_eq!(tool, "ver");
                assert_eq!(args, json!({"rows": 5}));
            }
            other => panic!("{other:?}"),
        }
        // probe: [op][bytes u32]
        assert_eq!(
            decode(&[0x03, 0, 0, 16, 0]).unwrap(),
            Some(Request::Probe { bytes: 4096 })
        );
        // ack: [op][bytes u32][delay_ms i32] — 9 bytes, fixed offsets.
        assert_eq!(
            decode(&[0x04, 0, 0, 5, 0, 0, 0, 0, 7]).unwrap(),
            Some(Request::Ack {
                bytes: 1280,
                delay_ms: 7
            })
        );
    }

    #[test]
    fn a_small_reply_counts_its_header_against_the_flash_room() {
        let room = Channel::Flash.room();
        assert!(fits_one_flash(room - OK_SMALL_HEADER, room));
        assert!(!fits_one_flash(room - OK_SMALL_HEADER + 1, room));
        assert!(!fits_one_flash(room, room));
    }

    #[test]
    fn a_bad_frame_is_a_named_refusal_not_a_panic_nor_silence() {
        assert_eq!(decode(&[0x09]).unwrap(), None);
        assert!(decode(&[]).is_err());
        assert!(decode(&[0x01, 1]).is_err());
        assert!(decode(&[0x02]).is_err());
        assert!(decode(&[0x04, 0, 0]).is_err());
        let why = refused(RefusedKind::Frame, "op byte 0x09 names no op");
        assert_eq!(why[0..4], [0x83, 1, 0, 24]);
        assert_eq!(&why[4..], b"op byte 0x09 names no op");
    }
}
