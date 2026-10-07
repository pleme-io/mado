# Grid ↔ Threading Contract (M2 Stream G — consumed at M7)

> **Status.** Types shaped in the single M2 grid touch
> (REMEDIATION-PLAN.md §M2, fold-in from Stream G). `DirtyRegion` and
> `GridDamage` ship in `src/grid_damage.rs` with bitset unit tests,
> under `#![allow(dead_code)]`: nothing consumes them yet.
> `ParseMailbox`/`Backpressure` are the typed sketch below and are not
> implemented. On the **local-PTY path** M7 consumes this contract and
> does not re-open Cell/Grid/SharedTerminal. On the **tear path** the
> render decouple, the damage and the mailbox belong to tear's
> [`docs/PERFORMANCE.md`](https://github.com/pleme-io/tear/blob/main/docs/PERFORMANCE.md)
> instead: R13 (the UI thread holds only a link), R30 (mado reads one
> frame, which turns the rows into `Arc<Line>` with a row id and a
> version) and R31 (a renderer that redraws what changed). M7 keeps the
> local-PTY path until PERFORMANCE R38 makes it an embedded tear pane.

> **Two runtimes.** Everything below about a PTY reader describes the
> **local-PTY runtime** (`tear.mode: never`, or `auto` when tear is
> unreachable), where mado's own reader owns the PTY. In the **tear
> runtimes** (`tear.runtime`: `embedded`, the default, `daemon` and
> `resident`) tear owns the PTY and its reader — in `embedded` that
> reader is a tear-core thread inside mado's process, otherwise it is
> in the tear daemon or a holder — and mado's mirror `Terminal` parses
> what tear forwards on the AppKit main thread; see
> [the tear runtimes](#the-tear-runtimes) below. The tear ↔ mado path —
> the UI thread, the wake path, frame pacing, the view lane, attach and
> switch — is measured and planned in PERFORMANCE.md.

## Why this exists

M7's threading work (bounded parse mailbox + coalesce-redraw, then
the render-thread decouple) needs two things from the grid that must
be *designed in* during the one allowed grid touch, even though they
are *wired* later:

1. **A damage vocabulary** — what "these rows changed" means in a
   world where the parser and the renderer stop sharing one big
   `RwLock<Terminal>` critical section per frame.
2. **A bounded hand-off** — what happens when the PTY floods faster
   than the renderer drains.

The M2 grid is already shaped for both: every read path goes through
methods (`rows_from` / `visible_rows_iter` / `viewport_rows`), rows
are typed `Line` values with stable `LogicalLineId`s, and nothing
outside `Grid` touches `VecDeque` internals.

## GridDamage / DirtyRegion (shipped types — `src/grid_damage.rs`)

```rust
/// Per-row dirty bitset over the viewport. O(rows/64) ops; spans()
/// coalesces adjacent dirty rows into renderer draw units.
pub struct DirtyRegion { bits: Vec<u64>, rows: usize }

impl DirtyRegion {
    pub fn new(rows: usize) -> Self;
    pub fn mark(&mut self, row: usize);              // OOB = no-op
    pub fn mark_range(&mut self, range: Range<usize>);
    pub fn mark_all(&mut self);
    pub fn clear(&mut self);                          // drained per frame
    pub fn clear_row(&mut self, row: usize);
    pub fn is_dirty(&self, row: usize) -> bool;
    pub fn any(&self) -> bool;
    pub fn count(&self) -> usize;
    pub fn union(&mut self, other: &Self);            // batch → frame
    pub fn spans(&self) -> Vec<Range<usize>>;         // coalesced redraw units
    pub fn resize(&mut self, rows: usize);            // resize ⇒ all dirty
}

/// One frame's damage — parse thread → render thread.
pub enum GridDamage {
    Full,                                             // resize / palette / alt flip
    Rows(DirtyRegion),                                // exactly these rows
    Scrolled { region: Range<usize>, lines: usize, dirty: DirtyRegion },
}
```

Contract points:

- Damage is **viewport-row granular** (not cell-granular). A row is
  the renderer's natural batch (one glyph run rebuild); cell-level
  bitsets cost more to maintain than they save.
- `Scrolled` exists because streaming output is dominated by
  full-screen scrolls: blit the region up `lines` rows, redraw only
  `dirty` (typically the one new bottom row). Without this arm a
  `cat largefile` marks every row every frame.
- `union` is how per-parse-batch damage accumulates into one frame's
  damage under the mailbox — the flood test in §M7 ("one
  damaged-region redraw per frame, not N") is exactly
  `frame.union(batch)` called N times and `spans()` drained once.
- Mismatched-size `union` overapproximates to all-dirty rather than
  panicking: a resize may race a batch by design.

## ParseMailbox / Backpressure (typed sketch — lands at M7)

```rust
/// Bounded SPSC hand-off: PTY reader thread → parser.
/// Replaces today's unbounded "reader locks Terminal and feeds
/// whatever arrived" with a typed, observable queue.
pub struct ParseMailbox {
    /// Ring of raw PTY chunks awaiting parse. Bounded: when full,
    /// the reader applies `Backpressure`.
    chunks: ringbuf::HeapRb<PtyChunk>,
    /// Damage accumulated by the parser since the renderer last
    /// drained — unioned per batch, drained once per frame.
    pending_damage: GridDamage,
}

pub struct PtyChunk {
    pub bytes: Box<[u8]>,
    pub received_at: std::time::Instant,   // flood observability
}

/// What the PTY reader does when the mailbox is full.
pub enum Backpressure {
    /// Stop reading the PTY fd — the kernel buffer (and ultimately
    /// the foreground process via blocked write()) absorbs the
    /// flood. The parser that owns the PTY NEVER drops bytes
    /// (correctness floor: dropped bytes = corrupted escape
    /// sequences = wrong grid).
    PauseReader,
}

impl ParseMailbox {
    pub fn push(&mut self, chunk: PtyChunk) -> Result<(), Backpressure>;
    pub fn parse_available(&mut self, term: &mut Terminal) -> GridDamage;
    pub fn drain_damage(&mut self) -> GridDamage;      // renderer, 1×/frame
}
```

Contract points:

- **Never drop bytes.** `Backpressure` has exactly one arm by
  design: pausing the reader is the only correct response (the
  kernel PTY buffer blocks the writer — the same flow control every
  real terminal relies on). Coalescing/dropping *bytes* is
  unrepresentable; only *redraws* coalesce. The rule binds the
  **authority**, the parser that owns the PTY: mado's `Terminal` in
  the local-PTY runtime, tear's `PaneGrid` in the tear runtimes. A
  view of an authority may skip bytes and resync from it, as
  PERFORMANCE R22 and R34 specify.
- The mailbox owns `pending_damage`, so the renderer takes the lock
  for `drain_damage()` + row reads only — not for the whole parse.

## The tear runtimes

In the tear runtimes the PTY reader is tear's, not mado's event
loop's:

- **embedded** (the default): tear-core's `tear-pty-reader` thread,
  inside mado's process (`tear-core/src/pty.rs`), reads the PTY and
  feeds every chunk to the pane's `PaneGrid` under its mutex before
  fanning it out to subscribers (`tear-core/src/inproc.rs`, the pane
  callback). That thread is the authority's reader and the place
  `PauseReader` pauses.
- **daemon** and **resident**: the reader is in the tear daemon or the
  pane's holder, and no PTY is open in mado's process.

In all three, chunks reach the window from a tear subscription over
unbounded channels, and its event loop drains them on the AppKit main
thread, up to 4,096 messages a tick (`gui_tear_attach.rs`, the
`poll_one` loop), each one VT-parsed again into mado's mirror
`Terminal` before the frame. That second parse is on the main thread,
not off it, and the mailbox above is not the next step for it:

- backpressure is implemented at the source, in tear's PTY reader,
  where `PauseReader` is the only arm (PERFORMANCE R21, whose local
  half — the in-process source embedded uses — lands first);
- subscriber queues become bounded, and a lagging view resyncs from
  the authority instead of queueing without bound (R22, R34);
- the parse leaves the UI thread for a view-lane thread (R13), and the
  renderer reads an immutable frame (R30) and redraws only changed
  rows (R31), consuming this document's damage vocabulary.

## Why the render decouple defers to M7

This section is about the local-PTY runtime; on the tear path the
decouple is PERFORMANCE R13. The riskiest piece — moving rendering
off the thread that owns the event loop — **fights madori's
`RenderCallback` ownership** (the
render closure is called by the platform layer with `&mut` access on
the main thread). Re-architecting that boundary now would couple the
M2 grid touch to a windowing-layer refactor with its own incident
surface (REMEDIATION-PLAN.md §M7 sequences it after the bounded
mailbox + coalesce-redraw prove out, behind a *named* milestone).
M2's obligation is only that the grid *never needs to be touched
again* when that lands — hence: damage vocabulary defined here,
per-row reads already behind methods, and the seqno/dirty protocol
(`Terminal::dirty()`) untouched and compatible (a `GridDamage::Full`
is exactly today's semantics, so M7 can adopt incrementally —
whole-frame first, spans after).

## Arc<Line> CoW scrollback (groundwork note)

`Grid.rows` is `VecDeque<Line>` and **no caller iterates the
VecDeque directly** — every read goes through `rows_from` /
`visible_rows_iter` / `viewport_rows` / `visible_row(_mut)`. The
planned paged/CoW scrollback (M2 groundwork, post-M7 wiring) swaps
the element type to `Arc<Line>`:

- readers are untouched (`&Line` still comes out of the iterators);
- writers go through `visible_line_mut` / `visible_row_mut`, which
  become `Arc::make_mut` call sites — scrollback rows shared with a
  snapshot/page are copied only on the (rare) write;
- `LogicalLineId` is `Copy` and survives the clone, so
  rewrap/mark-anchoring semantics are unchanged.

That makes snapshot-for-search and paged eviction O(1) clones of row
handles instead of deep copies — without a third grid touch.
