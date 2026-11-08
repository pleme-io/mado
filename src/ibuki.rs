//! mado's ibuki-compliant communication border.
//!
//! A docked blue program with a terminal face runs in a mado session
//! (theory/IBUKI.md §V.2), and its window-side communication — the world
//! answer of `mado blue-world`, the effects of `mado blue-command` — crosses
//! this border. The wire frames, the Flash bucket and the MTU are blue's
//! own (`blue_lang_runtime::wire`, the crate `blue-lang-host` already
//! compiles in): mado CONSUMES that contract rather than re-authoring it,
//! so every framing byte and every rate arithmetic below is the primitive's.
//! What is mado's is the border itself — the reply channel between a
//! window process and its workers, carried over the worker's stdout pipe:
//!
//! - the reply body is CANONICAL JSON (recursively sorted keys, compact —
//!   the form blue hashes, §III.7), chunked into frames no datagram-sized
//!   carrier could refuse (≤ the wire MTU of 1280, header included);
//! - every frame is a Flash frame (§III.6: control-plane traffic — the
//!   world overlay is interactive configuration, not bulk scavenging) and
//!   the sender's spends cross the host Flash bucket (§III.8), so a reply
//!   larger than one burst is paced, and a reply no wait can fit is a typed
//!   refusal rather than a silent truncation;
//! - a pipe preserves no datagram boundaries, so each frame travels in a
//!   two-byte big-endian length envelope — exactly the length a UDP
//!   carrier would carry, and nothing more;
//! - the reader refuses what is not a frame, names the channel it would
//!   not carry, and falls back to the workers' former one-line plain JSON
//!   so an unframed answer is still read (additive, §behaviour-configured-off).
//!
//! The Tide controller (RFC 6817 LEDBAT, §III.3) is the wire's other
//! channel; it lives here in Rust because blue's is a bidama the host
//! border cannot call. Its arithmetic mirrors `blue/bidamas/ibuki/ibuki.b`
//! — the doctests there are this module's pinned tests.

use std::io::Write;

use blue_lang_runtime::wire::{self, Bucket, Channel};
use serde_json::Value;

/// One frame plus the two bytes that carry its length on a stream.
pub const ENVELOPE_HEAD: usize = 2;

/// The Flash bucket's rate at mado's border: the wire default, 16 KiB/s
/// with a one-second burst (§III.8).
#[must_use]
pub fn border_rate() -> u64 {
    wire::DEFAULT_FLASH_RATE
}

/// Canonical JSON: object keys sorted recursively, compact separators —
/// the same form `blue_lang_runtime::json::canonical_text` produces, built
/// here from a `serde_json::Value` so the reply never round-trips through
/// text twice.
#[must_use]
pub fn canonical_json(reply: &Value) -> String {
    serde_json::to_string(&sorted(reply.clone())).unwrap_or_default()
}

fn sorted(v: Value) -> Value {
    match v {
        Value::Array(xs) => Value::Array(xs.into_iter().map(sorted).collect()),
        Value::Object(m) => {
            let mut entries: Vec<(String, Value)> =
                m.into_iter().map(|(k, v)| (k, sorted(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        other => other,
    }
}

/// Chunk a payload into Flash payloads the wire will carry: each chunk is
/// at most `Channel::Flash`'s room inside the MTU (1279 bytes).
#[must_use]
pub fn flash_chunks(payload: &[u8]) -> Vec<&[u8]> {
    let room = Channel::Flash.room();
    payload.chunks(room.max(1)).collect()
}

/// Wrap one frame in its stream-carrier envelope: two big-endian length
/// bytes, then the frame.
pub fn envelope(frame: &[u8], out: &mut Vec<u8>) {
    let len = u16::try_from(frame.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(frame);
}

/// Why a framed reply could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BorderRefusal {
    /// The stream does not begin with a frame envelope — the caller may
    /// read it as the workers' former plain one-line JSON instead.
    NotFramed,
    /// An envelope names a length past the wire MTU (with its own head).
    LengthPastMtu(usize),
    /// The stream ends inside an envelope or its frame.
    Truncated { read: usize, named: usize },
    /// The bytes inside an envelope are not a wire frame.
    Decode(String),
    /// The reply must ride Flash frames; this names the other channel.
    TideInReply { stamp_ms: i64 },
}

/// Split a stream back into the frames its envelopes carry.
fn frames_enveloped(bytes: &[u8]) -> Result<Vec<Vec<u8>>, BorderRefusal> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        if at + ENVELOPE_HEAD > bytes.len() {
            return Err(BorderRefusal::Truncated {
                read: bytes.len() - at,
                named: ENVELOPE_HEAD,
            });
        }
        let head = [bytes[at], bytes[at + 1]];
        let named = usize::from(u16::from_be_bytes(head));
        if named > wire::MTU {
            return Err(BorderRefusal::LengthPastMtu(named));
        }
        let end = at + ENVELOPE_HEAD + named;
        if end > bytes.len() {
            return Err(BorderRefusal::Truncated {
                read: bytes.len() - at - ENVELOPE_HEAD,
                named,
            });
        }
        out.push(bytes[at + ENVELOPE_HEAD..end].to_vec());
        at = end;
    }
    Ok(out)
}

/// Read a framed reply: every envelope must decode to a wire frame, every
/// frame must be Flash, and the concatenated payloads are the reply body.
pub fn framed_payload(bytes: &[u8]) -> Result<Vec<u8>, BorderRefusal> {
    let frames = frames_enveloped(bytes)?;
    if frames.is_empty() {
        return Err(BorderRefusal::NotFramed);
    }
    let mut payload = Vec::new();
    for raw in frames {
        let frame = wire::decode(&raw).map_err(|e| BorderRefusal::Decode(e.to_string()))?;
        match frame.channel {
            Channel::Flash => payload.extend_from_slice(&frame.payload),
            Channel::Tide => {
                return Err(BorderRefusal::TideInReply {
                    stamp_ms: frame.stamp_ms.unwrap_or_default(),
                });
            }
        }
    }
    Ok(payload)
}

/// What became of a reply the border was asked to carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// The reply crossed as this many Flash frames of this many body bytes.
    Written { frames: usize, body: usize },
    /// The Flash bucket refused a frame even after its wait was waited
    /// out; the reply did not cross and nothing of it was sent past the
    /// frames already written.
    Refused {
        bytes: usize,
        rate: u64,
        burst: u64,
        wait_ms: u64,
    },
}

/// The sender side of the border: canonical JSON, Flash frames, the host
/// bucket pacing every spend. `wait_out` sleeps a refused frame's wait so
/// the caller's clock stays the caller's (tests pass a no-op and step the
/// clock instead).
pub struct ReplySender<'a> {
    out: &'a mut dyn Write,
    bucket: Bucket,
}

impl<'a> ReplySender<'a> {
    #[must_use]
    pub fn new(out: &'a mut dyn Write, rate: u64) -> Self {
        Self {
            out,
            bucket: Bucket::new(rate),
        }
    }

    /// Send `reply` as canonical JSON in Flash frames. A frame the bucket
    /// refuses is retried once after its `wait_ms`; a second refusal is
    /// returned as [`Sent::Refused`] with nothing further written.
    pub fn send(
        &mut self,
        reply: &Value,
        now_ms: i64,
        wait_out: &mut dyn FnMut(u64),
    ) -> std::io::Result<Sent> {
        let body = canonical_json(reply).into_bytes();
        let mut frames = 0usize;
        let mut written = 0usize;
        let mut now = now_ms;
        for chunk in flash_chunks(&body) {
            let frame = wire::encode(Channel::Flash, 0, chunk)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            if let Err(first) = self.bucket.spend(frame.len(), now) {
                let Some(wait) = first.wait_ms else {
                    return Ok(refused_of(first, frame.len()));
                };
                wait_out(wait);
                now += i64::try_from(wait).unwrap_or(i64::MAX);
                if let Err(second) = self.bucket.spend(frame.len(), now) {
                    return Ok(refused_of(second, frame.len()));
                }
            }
            let mut buf = Vec::with_capacity(ENVELOPE_HEAD + frame.len());
            envelope(&frame, &mut buf);
            self.out.write_all(&buf)?;
            written += chunk.len();
            frames += 1;
        }
        self.out.flush()?;
        Ok(Sent::Written {
            frames,
            body: written,
        })
    }
}

fn refused_of(r: blue_lang_runtime::wire::RateRefusal, bytes: usize) -> Sent {
    Sent::Refused {
        bytes,
        rate: r.rate,
        burst: r.burst,
        wait_ms: r.wait_ms.unwrap_or_default(),
    }
}

/// The `Load` a framed worker reply carries, or `None` when the stream is
/// not framed (the plain form — read it with [`plain_load`]).
#[must_use]
pub fn framed_load(stdout: &[u8]) -> Option<blue_lang_host::world::Load> {
    let payload = framed_payload(stdout).ok()?;
    let reply: Value = serde_json::from_str(String::from_utf8_lossy(&payload).trim()).ok()?;
    reply_load(&reply)
}

/// The `Load` of the workers' former plain one-line JSON answer, or `None`.
#[must_use]
pub fn plain_load(stdout: &[u8]) -> Option<blue_lang_host::world::Load> {
    let text = String::from_utf8_lossy(stdout);
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    let reply: Value = serde_json::from_str(line).ok()?;
    reply_load(&reply)
}

fn reply_load(reply: &Value) -> Option<blue_lang_host::world::Load> {
    use blue_lang_host::world::Load;
    if let Some(judged) = reply.get("judged") {
        return Some(Load::Judged(judged.clone()));
    }
    let refused = reply.get("refused")?;
    Some(Load::Refused(blue_lang_host::world::Refusal::new(
        refused.get("kind")?.as_str()?,
        refused.get("why")?.as_str()?,
    )))
}

/// Run a worker bounded and read its reply: framed first, the plain line
/// second, and every other end a typed `Load::Refused`. The ONE reader for
/// the window process and for the integration tests.
pub fn spawn_and_load(
    command: std::process::Command,
    limit: std::time::Duration,
) -> blue_lang_host::world::Load {
    use blue_lang_host::world::{Load, Refusal};
    match blue_lang_host::world::run_bounded(command, limit) {
        blue_lang_host::world::Bounded::Finished { stdout, status } => framed_load(&stdout)
            .or_else(|| plain_load(&stdout))
            .unwrap_or_else(|| {
                Load::Refused(Refusal::new(
                    "died",
                    format!("the world worker ended: {status}"),
                ))
            }),
        blue_lang_host::world::Bounded::TimedOut => Load::Refused(Refusal::new(
            "killed",
            format!("the world gave no answer in {} ms", limit.as_millis()),
        )),
        blue_lang_host::world::Bounded::Failed(e) => {
            Load::Refused(Refusal::new("spawn", e.to_string()))
        }
    }
}

/// The reply a worker frames, in the shape the window reads: `judged`
/// carries the world answer, `refused` its refusal — the same two shapes
/// the plain one-line protocol carries, so the reader of either form
/// lands in the same place.
#[must_use]
pub fn reply_value(judged: &Result<Value, blue_lang_host::world::Refusal>) -> Value {
    match judged {
        Ok(judged) => serde_json::json!({ "judged": judged }),
        Err(r) => serde_json::json!({ "refused": { "kind": r.kind, "why": r.why } }),
    }
}

// ── Tide, the LEDBAT controller (RFC 6817) ─────────────────────────────
//
// A Rust mirror of `blue/bidamas/ibuki/ibuki.b`'s tide: a target of
// queuing delay, a gain, an MSS, a floor and initial window in packets,
// an allowed increase per ack, a base-delay history in minutes, a filter
// of current delays, and a collapse threshold at which the window falls
// to its floor. mado holds it for the bulk seam (§III.3); the reply
// channel rides Flash.

/// One measured acknowledgement: `bytes` acked, the one-way `delay_ms`,
/// the bytes in `flight` when they were sent, at `now_ms`.
#[allow(dead_code)]
// the Tide seam's consumer is the bulk channel (§III.3); the reply border rides Flash, and the pins below keep the contract alive until it lands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    pub bytes: u64,
    pub delay_ms: i64,
    pub flight: Option<i64>,
    pub now_ms: i64,
}

/// A Tide channel's controller. Defaults mirror the bidama: target 100 ms,
/// gain 1, MSS 1280, floor and initial window of 2 packets, allowed
/// increase 1 packet, 10 minutes of base history, a filter of 4, collapse
/// at 2 ms of queuing delay.
#[allow(dead_code)] // held for the bulk channel; every pin below is a test the compiler cannot see from the lib build
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tide {
    target_ms: i64,
    gain: i64,
    mss: i64,
    min_cwnd: i64,
    allowed_increase: i64,
    base_minutes: usize,
    filter: usize,
    collapse_ms: Option<i64>,
    cwnd: i64,
    base: Vec<(u64, i64)>,
    current: Vec<i64>,
}

impl Default for Tide {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)] // the bulk seam's contract; the pins below are its tests
impl Tide {
    #[must_use]
    pub fn new() -> Self {
        Self {
            target_ms: 100,
            gain: 1,
            mss: 1280,
            min_cwnd: 2,
            allowed_increase: 1,
            base_minutes: 10,
            filter: 4,
            collapse_ms: Some(2),
            cwnd: 2 * 1280,
            base: Vec::new(),
            current: Vec::new(),
        }
    }

    /// `collapse_ms: nil` — the window only ever moves by the formula,
    /// never falling to its floor on queuing delay alone.
    #[must_use]
    pub fn no_collapse() -> Self {
        Self {
            collapse_ms: None,
            ..Self::new()
        }
    }

    /// The smallest window a Tide keeps, in bytes (2 packets, 2560).
    #[must_use]
    pub fn floor_window(&self) -> i64 {
        self.min_cwnd * self.mss
    }

    /// The window as it stands, in bytes.
    #[must_use]
    pub fn cwnd(&self) -> i64 {
        self.cwnd
    }

    /// The queuing delay last measured: the least of the current delays
    /// less the least base delay; `None` before any sample.
    #[must_use]
    pub fn queuing(&self) -> Option<i64> {
        if self.current.is_empty() {
            return None;
        }
        let cur = self.current.iter().copied().min()?;
        let base = self.base.iter().map(|&(_, d)| d).min()?;
        Some(cur - base)
    }

    /// The per-minute minima of one-way delay, the newest `keep` minutes.
    fn base_update(base: Vec<(u64, i64)>, minute: u64, delay: i64, keep: usize) -> Vec<(u64, i64)> {
        let mut base = base;
        match base.last() {
            Some(&(last_minute, _)) if last_minute == minute => {
                let (_, prev) = base.pop().unwrap_or((minute, delay));
                base.push((minute, prev.min(delay)));
            }
            _ => {
                base.push((minute, delay));
                let excess = base.len().saturating_sub(keep);
                base.drain(..excess);
            }
        }
        base
    }

    /// The Tide after an ack: the window moves toward the target queuing
    /// delay, at most `allowed_increase` packets past the flight, never
    /// under its floor, and falls to the floor at `collapse_ms`.
    #[must_use]
    pub fn ack(mut self, ack: Ack) -> Self {
        let minute = u64::try_from(ack.now_ms.div_euclid(60_000)).unwrap_or(0);
        self.base = Self::base_update(self.base, minute, ack.delay_ms, self.base_minutes);
        self.current.push(ack.delay_ms);
        let excess = self.current.len().saturating_sub(self.filter);
        self.current.drain(..excess);
        let Some(q) = self.queuing() else {
            return self;
        };
        let lowest = self.floor_window();
        if self.collapse_ms.is_some_and(|collapse| q >= collapse) {
            self.cwnd = lowest;
            return self;
        }
        let flight = ack.flight.unwrap_or(self.cwnd);
        let grown = self.cwnd
            + floor_div(
                self.gain * (self.target_ms - q) * ack.bytes as i64 * self.mss,
                self.target_ms * self.cwnd,
            );
        self.cwnd = grown
            .min(flight + self.allowed_increase * self.mss)
            .max(lowest);
        self
    }

    /// The Tide after a loss: its window halved, never under its floor.
    #[must_use]
    pub fn loss(mut self) -> Self {
        self.cwnd = self.cwnd.min((self.cwnd / 2).max(self.floor_window()));
        self
    }
}

/// Division rounded toward negative infinity, the floor the bidama's
/// `floor()` applies over its exact quotient.
fn floor_div(num: i64, den: i64) -> i64 {
    debug_assert!(den > 0);
    let q = num / den;
    if num % den != 0 && ((num < 0) != (den < 0)) {
        q - 1
    } else {
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blue_lang_runtime::wire::{Channel as Ch, decode, encode};
    use serde_json::json;

    fn frame_of(channel: Ch, stamp_ms: i64, payload: &[u8]) -> Vec<u8> {
        encode(channel, stamp_ms, payload).unwrap()
    }

    #[test]
    fn flash_and_tide_framing_bytes_are_the_wire_s() {
        let flash = frame_of(Ch::Flash, 0, b"hi");
        assert_eq!(flash, vec![0x02, b'h', b'i']);
        let tide = frame_of(Ch::Tide, 7, b"hi");
        assert_eq!(tide, vec![0x01, 0, 0, 0, 0, 0, 0, 0, 7, b'h', b'i']);
        let back = decode(&flash).unwrap();
        assert_eq!(back.channel, Ch::Flash);
        assert_eq!(back.payload, b"hi".to_vec());
        let back = decode(&tide).unwrap();
        assert_eq!(back.stamp_ms, Some(7));
    }

    #[test]
    fn a_frame_past_the_mtu_is_refused_both_ways() {
        let full = frame_of(Ch::Flash, 0, &vec![b'x'; Ch::Flash.room()]);
        assert_eq!(full.len(), wire::MTU);
        assert!(encode(Ch::Flash, 0, &vec![b'x'; Ch::Flash.room() + 1]).is_err());
        assert!(decode(&vec![0x02; wire::MTU + 1]).is_err());
        assert!(decode(&[]).is_err());
        assert!(decode(&[0x03, b'x']).is_err());
        assert!(decode(&[0x01, 0, 0]).is_err());
    }

    #[test]
    fn the_border_bucket_sends_twelve_full_frames_and_names_sixteen_ms() {
        let mut b = Bucket::new(wire::DEFAULT_FLASH_RATE);
        for _ in 0..12 {
            assert!(b.spend(wire::MTU, 0).is_ok());
        }
        let refusal = b.spend(wire::MTU, 0).unwrap_err();
        assert_eq!(refusal.wait_ms, Some(16));
        assert_eq!(refusal.rate, wire::DEFAULT_FLASH_RATE);
        assert_eq!(refusal.burst, wire::DEFAULT_FLASH_RATE);
    }

    #[test]
    fn a_clock_that_steps_back_adds_and_takes_nothing() {
        let mut b = Bucket::new(1000);
        assert!(b.spend(1000, 5000).is_ok());
        let before = b.tokens();
        assert!(b.spend(1, 4000).is_err());
        assert_eq!(b.tokens(), before);
    }

    #[test]
    fn no_seeded_run_spends_more_than_burst_plus_rate_times_elapsed() {
        let mut seed = 0x5eed_1234_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..50 {
            let rate = 1 + next() % 20_000;
            let mut b = Bucket::new(rate);
            let mut now = 0_i64;
            let mut spent = 0_u64;
            for _ in 0..40 {
                now += (next() % 50) as i64;
                let bytes = 1 + (next() % 2000) as usize;
                if b.spend(bytes, now).is_ok() {
                    spent += bytes as u64;
                }
            }
            let elapsed = u64::try_from(now).unwrap_or(0);
            assert!(
                spent <= rate + rate.saturating_mul(elapsed),
                "spent {spent} > burst+rate*elapsed at rate {rate}, elapsed {elapsed}"
            );
        }
    }

    #[test]
    fn tide_grows_sixty_bytes_at_zero_queueing() {
        let t = Tide::new();
        assert_eq!(t.cwnd(), 2560);
        let t = t.ack(Ack {
            bytes: 1280,
            delay_ms: 50,
            flight: None,
            now_ms: 0,
        });
        assert_eq!(t.queuing(), Some(0));
        assert_eq!(t.cwnd(), 3200);
    }

    #[test]
    fn tide_shrinks_by_the_formula_at_two_hundred_ms_queueing() {
        // Seed the base at 50 ms, then age the seed out of the current
        // filter (4 samples) with 250 ms delays, so queuing measures 200.
        let mut t = Tide::no_collapse().ack(Ack {
            bytes: 1280,
            delay_ms: 50,
            flight: None,
            now_ms: 0,
        });
        for now in 1..5 {
            t = t.ack(Ack {
                bytes: 1280,
                delay_ms: 250,
                flight: None,
                now_ms: now,
            });
        }
        assert_eq!(t.queuing(), Some(200));
        let c0 = t.cwnd();
        let t = t.ack(Ack {
            bytes: 1280,
            delay_ms: 250,
            flight: Some(c0),
            now_ms: 5,
        });
        // RFC 6817 over (target − q) with mss 1280, gain 1, target 100:
        // floor toward −infinity of −(100 × 1280 × 1280) / (100 × c0).
        let num = -(100_i64 * 1280 * 1280);
        let den = 100_i64 * c0;
        let q = num / den;
        let floor = if num % den != 0 { q - 1 } else { q };
        assert_eq!(t.cwnd(), c0 + floor);
        assert!(t.cwnd() >= t.floor_window());
    }

    #[test]
    fn tide_collapses_to_its_floor_two_ms_past_its_base() {
        let mut t = Tide::new().ack(Ack {
            bytes: 1280,
            delay_ms: 50,
            flight: None,
            now_ms: 0,
        });
        for now in 1..5 {
            t = t.ack(Ack {
                bytes: 1280,
                delay_ms: 52,
                flight: None,
                now_ms: now,
            });
        }
        assert_eq!(t.queuing(), Some(2));
        assert_eq!(t.cwnd(), 2560);
    }

    #[test]
    fn a_stretch_ack_grows_the_window_by_one_packet_at_most() {
        let t = Tide::new();
        let t = t.ack(Ack {
            bytes: 10 * 1280,
            delay_ms: 50,
            flight: Some(2560),
            now_ms: 0,
        });
        assert!(t.cwnd() <= 2560 + 1280, "cwnd {}", t.cwnd());
    }

    #[test]
    fn an_application_limited_sender_does_not_grow_the_window() {
        let t = Tide::new();
        let t = t.ack(Ack {
            bytes: 1280,
            delay_ms: 50,
            flight: Some(1),
            now_ms: 0,
        });
        assert_eq!(t.cwnd(), t.floor_window());
    }

    #[test]
    fn a_loss_halves_the_window_never_under_its_floor() {
        let mut t = Tide::new();
        t = t.ack(Ack {
            bytes: 1280,
            delay_ms: 50,
            flight: Some(12800),
            now_ms: 0,
        });
        // Push the window up through several acks so a halving has room.
        for now in 1..5 {
            t = t.ack(Ack {
                bytes: 1280,
                delay_ms: 50,
                flight: None,
                now_ms: now,
            });
        }
        let before = t.cwnd();
        let t = t.loss();
        assert_eq!(t.cwnd(), (before / 2).max(2560));
        let t = t.loss().loss();
        assert_eq!(t.cwnd(), 2560);
    }

    #[test]
    fn the_window_never_leaves_its_bounds_over_seeded_acks_and_losses() {
        let mut seed = 0xfeed_beef_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..40 {
            let mut t = Tide::new();
            for i in 0..60 {
                let delay = 40 + (next() % 180) as i64;
                t = t.ack(Ack {
                    bytes: 1280 * (1 + next() % 3),
                    delay_ms: delay,
                    flight: Some((1 + next() % 40_000) as i64),
                    now_ms: i,
                });
                if next() % 7 == 0 {
                    t = t.loss();
                }
                assert!(t.cwnd() >= t.floor_window());
                assert!(t.cwnd() <= 40_000 + 1280);
            }
        }
    }

    #[test]
    fn canonical_json_sorts_every_object_recursively_and_is_idempotent() {
        let reply = json!({"b": 1, "a": {"z": [1, {"y": 2, "x": 3}], "w": null}});
        let once = canonical_json(&reply);
        assert_eq!(once, r#"{"a":{"w":null,"z":[1,{"x":3,"y":2}]},"b":1}"#);
        let twice = canonical_json(&serde_json::from_str::<Value>(&once).unwrap());
        assert_eq!(once, twice);
    }

    #[test]
    fn envelopes_round_trip_and_refuse_the_shapes_a_pipe_cannot_carry() {
        let frame = frame_of(Ch::Flash, 0, b"payload");
        let mut stream = Vec::new();
        envelope(&frame, &mut stream);
        let frames = frames_enveloped(&stream).unwrap();
        assert_eq!(frames, vec![frame.clone()]);
        // A plain JSON line's first two bytes (`{"`) name a length far
        // past the MTU, so the border reads it as unframed.
        assert_eq!(
            frames_enveloped(b"{\"judged\": []}\n").unwrap_err(),
            BorderRefusal::LengthPastMtu(0x7b22)
        );
        let mut truncated = stream.clone();
        truncated.pop();
        assert!(matches!(
            frames_enveloped(&truncated),
            Err(BorderRefusal::Truncated { .. })
        ));
    }

    #[test]
    fn a_framed_reply_reassembles_and_a_tide_frame_in_it_is_refused_by_name() {
        let reply = json!({"judged": {"font": {"size": 14}}});
        let body = canonical_json(&reply).into_bytes();
        let mut stream = Vec::new();
        for chunk in flash_chunks(&body) {
            envelope(&frame_of(Ch::Flash, 0, chunk), &mut stream);
        }
        let payload = framed_payload(&stream).unwrap();
        assert_eq!(payload, body);
        let mut with_tide = stream.clone();
        envelope(&frame_of(Ch::Tide, 9, b"extra"), &mut with_tide);
        assert_eq!(
            framed_payload(&with_tide).unwrap_err(),
            BorderRefusal::TideInReply { stamp_ms: 9 }
        );
    }

    #[test]
    fn the_sender_frames_canonical_json_within_the_mtu_and_reports_it() {
        let reply = json!({"refused": {"kind": "load", "why": "no such word"}});
        let mut out = Vec::new();
        let mut no_wait = |_wait_ms: u64| {};
        let mut sender = ReplySender::new(&mut out, border_rate());
        let sent = sender.send(&reply, 0, &mut no_wait).unwrap();
        assert_eq!(
            sent,
            Sent::Written {
                frames: 1,
                body: canonical_json(&reply).len()
            }
        );
        let payload = framed_payload(&out).unwrap();
        assert_eq!(payload, canonical_json(&reply).into_bytes());
    }

    #[test]
    fn a_reply_past_one_burst_is_paced_frame_by_frame_under_a_stepped_clock() {
        let mut big = serde_json::Map::new();
        big.insert("padding".into(), Value::String("x".repeat(20 * 1024)));
        let reply = Value::Object(big);
        let mut waited = 0_u64;
        let mut wait = |ms: u64| {
            waited += ms;
        };
        let mut out = Vec::new();
        let mut sender = ReplySender::new(&mut out, border_rate());
        let sent = sender.send(&reply, 0, &mut wait).unwrap();
        let Sent::Written { frames, body } = sent else {
            panic!("a 20 KiB reply must fit the 10 s world deadline, not refuse");
        };
        assert_eq!(body, canonical_json(&reply).len());
        assert!(frames > 12, "{frames} frames for a 20 KiB reply");
        assert!(waited > 0, "a reply past the burst must have waited");
        let payload = framed_payload(&out).unwrap();
        assert_eq!(payload.len(), body);
        assert_eq!(payload, canonical_json(&reply).into_bytes());
    }

    #[test]
    fn the_same_reply_produces_the_same_bytes_twice() {
        let reply = json!({"judged": {"entries": [{"entry": "config", "name": "cursor", "value": {"blink": false}}]}});
        let mut first = Vec::new();
        let mut second = Vec::new();
        let mut no_wait = |_wait_ms: u64| {};
        ReplySender::new(&mut first, border_rate())
            .send(&reply, 0, &mut no_wait)
            .unwrap();
        ReplySender::new(&mut second, border_rate())
            .send(&reply, 0, &mut no_wait)
            .unwrap();
        assert_eq!(first, second);
    }
}
