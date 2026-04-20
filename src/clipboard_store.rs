//! Content-addressed clipboard store.

#![allow(dead_code)] // Some helpers (kind.label, entry.content clones) are only exercised by tests / future IPC.
//!
//! Invention. Every terminal implements OSC 52 as "set the system
//! clipboard to this string" — the payload vanishes into the OS
//! clipboard layer and becomes indistinguishable from whatever
//! else the user copied. `ClipboardStore` keeps a **session-local**
//! mirror of every OSC 52 payload mado sees, indexed by BLAKE3-128
//! hash of its content. That hash becomes a stable token the
//! clipboard payload can be referenced by after the fact:
//!
//!   1. Shell runs `printf '\e]52;c;<base64>\e\\'`.
//!   2. mado parses OSC 52, decodes base64, stores into
//!      `ClipboardStore` → gets back a `ClipboardHash`.
//!   3. mado emits that hash via an MCP tool (planned) so escriba
//!      (or any other typed client) can fetch the payload back
//!      by hash — **after** the OS clipboard has rotated on to
//!      a different copy.
//!
//! No editor category member exposes clipboard content-addressing.
//! Alacritty / iTerm2 / ghostty all forget the payload the moment
//! OSC 52 runs. Mado keeps the whole session's scrollback-of-
//! clipboards, letting workflows (escriba `defworkflow` steps, LLM
//! tools, scripts) quote and attest specific copies.
//!
//! # Semantics
//!
//! - **Idempotent** — storing the same content twice returns the
//!   same hash; the `set_at` timestamp refreshes to the latest
//!   write so LRU rotation reflects *most recent use*.
//! - **Capacity-bounded** — default 128 entries, oldest evicted
//!   first. Session-scoped; clearing is the implicit reset when
//!   the terminal exits.
//! - **Hash size = 128 bits** — 16 bytes of BLAKE3 output. More
//!   than enough for session uniqueness; keeps tokens short in
//!   MCP payloads (32 hex chars).

use std::collections::{HashMap, VecDeque};

/// 16-byte (128-bit) BLAKE3-prefix hash. Content-addressed
/// identifiers for clipboard payloads. `to_hex()` is the canonical
/// serialization for MCP wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClipboardHash([u8; 16]);

impl ClipboardHash {
    /// Compute the hash of `content`. Stable across sessions: same
    /// bytes in → same hash out. Short-prefix (16 bytes) collision
    /// risk is ~2^-64 for typical clipboard workloads; cheaper than
    /// the full 32-byte output at MCP-payload sizes.
    #[must_use]
    pub fn of(content: &str) -> Self {
        let full = blake3::hash(content.as_bytes());
        let mut short = [0u8; 16];
        short.copy_from_slice(&full.as_bytes()[..16]);
        Self(short)
    }

    /// 32-char lowercase hex encoding. The canonical wire form.
    #[must_use]
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(32);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Parse a 32-char hex string back into a hash. Returns `None`
    /// for anything else (wrong length, non-hex chars).
    #[must_use]
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 32 {
            return None;
        }
        let mut out = [0u8; 16];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self(out))
    }
}

/// Which OSC 52 "selection buffer" the payload targets. Vim /
/// emacs / tmux / ghostty all support these three; mado records
/// them so escriba workflows can say "give me the last *primary*
/// selection" rather than just "the last copy".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardKind {
    /// `c` — the system clipboard. Default.
    System,
    /// `p` — the primary (X11 middle-click) selection.
    Primary,
    /// `s` — secondary; rare but standards-compliant.
    Secondary,
}

impl ClipboardKind {
    /// Decode an OSC 52 kind byte. Defaults to [`System`](Self::System)
    /// for unrecognised / empty values.
    #[must_use]
    pub fn from_osc52_byte(b: &[u8]) -> Self {
        match b {
            b"p" | b"P" => Self::Primary,
            b"s" | b"S" => Self::Secondary,
            _ => Self::System,
        }
    }

    /// Canonical single-char label for logs / MCP payloads.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "c",
            Self::Primary => "p",
            Self::Secondary => "s",
        }
    }
}

/// One remembered clipboard copy.
#[derive(Debug, Clone)]
pub struct ClipboardEntry {
    pub hash: ClipboardHash,
    pub content: String,
    pub kind: ClipboardKind,
    /// Wall-clock-like monotonic timestamp — used purely for LRU
    /// ordering. We count `store()` invocations (not wall time) so
    /// tests stay deterministic and the type stays `no_std`-clean.
    pub set_at: u64,
}

/// Bounded LRU-eviction clipboard store, content-addressed.
#[derive(Debug)]
pub struct ClipboardStore {
    entries: HashMap<ClipboardHash, ClipboardEntry>,
    /// Insertion/access order; oldest at the front, most recent at
    /// the back. Eviction pops the front.
    order: VecDeque<ClipboardHash>,
    capacity: usize,
    /// Monotonic counter used as the `set_at` stamp.
    tick: u64,
}

impl ClipboardStore {
    /// New empty store bounded by `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
            tick: 0,
        }
    }

    /// Store `content` under its BLAKE3-prefix hash. Idempotent —
    /// calling twice with the same bytes returns the same hash,
    /// refreshes the LRU position, and updates `set_at`. Evicts
    /// the oldest entry if the store is at capacity and this is a
    /// fresh insert.
    pub fn store(&mut self, content: String, kind: ClipboardKind) -> ClipboardHash {
        let hash = ClipboardHash::of(&content);
        self.tick += 1;
        let already_present = self.entries.contains_key(&hash);
        if already_present {
            // Move to back of order — most recent.
            self.order.retain(|h| *h != hash);
        } else if self.entries.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.order.push_back(hash);
        self.entries.insert(
            hash,
            ClipboardEntry {
                hash,
                content,
                kind,
                set_at: self.tick,
            },
        );
        hash
    }

    /// Look up an entry by hash.
    #[must_use]
    pub fn get(&self, hash: ClipboardHash) -> Option<&ClipboardEntry> {
        self.entries.get(&hash)
    }

    /// Entries in most-recent-first order. The contract MCP tools
    /// that surface "clipboard history" consume.
    pub fn entries_recent_first(&self) -> impl Iterator<Item = &ClipboardEntry> {
        self.order
            .iter()
            .rev()
            .filter_map(|h| self.entries.get(h))
    }

    /// Count currently stored entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_across_calls() {
        let a = ClipboardHash::of("hello world");
        let b = ClipboardHash::of("hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn different_content_gives_different_hash() {
        let a = ClipboardHash::of("one");
        let b = ClipboardHash::of("two");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_round_trips_through_hex() {
        let h = ClipboardHash::of("payload");
        let hex = h.to_hex();
        assert_eq!(hex.len(), 32);
        assert_eq!(ClipboardHash::from_hex(&hex), Some(h));
    }

    #[test]
    fn hash_from_hex_rejects_malformed() {
        assert!(ClipboardHash::from_hex("too-short").is_none());
        assert!(ClipboardHash::from_hex(&"zz".repeat(16)).is_none());
    }

    #[test]
    fn kind_decoder_handles_canonical_bytes() {
        assert_eq!(ClipboardKind::from_osc52_byte(b"c"), ClipboardKind::System);
        assert_eq!(ClipboardKind::from_osc52_byte(b"p"), ClipboardKind::Primary);
        assert_eq!(ClipboardKind::from_osc52_byte(b"s"), ClipboardKind::Secondary);
        // Unknown → fallback to System (matches ghostty's permissive
        // parse; users who care about precision set `c` explicitly).
        assert_eq!(ClipboardKind::from_osc52_byte(b""), ClipboardKind::System);
        assert_eq!(ClipboardKind::from_osc52_byte(b"x"), ClipboardKind::System);
    }

    #[test]
    fn store_is_idempotent_on_same_content() {
        let mut store = ClipboardStore::new(16);
        let h1 = store.store("payload".into(), ClipboardKind::System);
        let h2 = store.store("payload".into(), ClipboardKind::System);
        assert_eq!(h1, h2);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_lru_evicts_oldest_first() {
        let mut store = ClipboardStore::new(2);
        let h_a = store.store("A".into(), ClipboardKind::System);
        let h_b = store.store("B".into(), ClipboardKind::System);
        let h_c = store.store("C".into(), ClipboardKind::System);
        assert_eq!(store.len(), 2);
        // A was oldest — evicted.
        assert!(store.get(h_a).is_none());
        assert!(store.get(h_b).is_some());
        assert!(store.get(h_c).is_some());
    }

    #[test]
    fn store_restore_moves_entry_to_back() {
        // Re-storing an existing hash bumps its LRU position — the
        // next eviction should skip it.
        let mut store = ClipboardStore::new(2);
        let h_a = store.store("A".into(), ClipboardKind::System);
        let _h_b = store.store("B".into(), ClipboardKind::System);
        // Re-touch A so it's most-recent.
        let _h_a2 = store.store("A".into(), ClipboardKind::System);
        // Fresh insert C evicts B (not A).
        let h_c = store.store("C".into(), ClipboardKind::System);
        assert!(store.get(h_a).is_some());
        assert!(store.get(h_c).is_some());
    }

    #[test]
    fn entries_recent_first_yields_newest_first() {
        let mut store = ClipboardStore::new(16);
        store.store("first".into(), ClipboardKind::System);
        store.store("second".into(), ClipboardKind::System);
        store.store("third".into(), ClipboardKind::System);
        let contents: Vec<String> = store
            .entries_recent_first()
            .map(|e| e.content.clone())
            .collect();
        assert_eq!(contents, vec!["third", "second", "first"]);
    }

    #[test]
    fn store_tracks_kind_per_entry() {
        let mut store = ClipboardStore::new(16);
        let h = store.store("x".into(), ClipboardKind::Primary);
        let entry = store.get(h).unwrap();
        assert_eq!(entry.kind, ClipboardKind::Primary);
        assert_eq!(entry.kind.label(), "p");
    }
}
