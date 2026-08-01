# Mado (窓) — GPU-Rendered Terminal Emulator


> **Both sibling pins are current as of 2026-07-31** — `tear@1c1007d` and
> `izumi@c2b48c0`. For one session each pin lagged its own absorbed source
> on purpose, because committing a lock that names an unfetchable rev is
> worse than one naming a stale rev; verification ran under a `--config
> patch` override until the siblings were pushed.
>
> Two things learned there, kept because they will recur:
>
> **A `--config patch` override is not sticky.** Any plain `cargo`
> invocation interleaved with the patched ones re-resolves against the
> pinned rev and silently rewrites `Cargo.lock` back. It surfaces as a
> genuine `no method named …` against the old crate, NOT as
> `Patch … was not used` — so the obvious check does not catch it.
>
> **Absorbing a signature ahead of its pin makes the tree red in exactly one
> direction.** Bumping izumi alone still failed with `this method takes 4
> arguments but 5 arguments were supplied`, because the tear pin had not
> moved yet. Both halves have to land together; the compiler is what
> enforces it, not the note.
A GPU-accelerated terminal emulator built in pure Rust. Follows Ghostty's philosophy
of speed + features + native UI without compromise, plus an embedded MCP server,
an embedded vigy reconciler, and deep Nix integration that no competitor offers.

> **★ Macro vocabulary — generate the problem space, don't derive everything.**
> mado is the worked reference for the org ★★ EMITTER SUBSTRATE refinement: the
> real macro leverage is **domain tables** (the VT/CSI/OSC/SGR dispatch — one
> authored table generates parse + emit + report, killing whole drift classes;
> e.g. `dec_private_modes!` in `terminal.rs`) and **genuine impl-duplication**
> (an enum's real `slug()`/`ALL` match → `KindStr`/`AllVariants`). Blanket
> per-field/per-variant deriving onto mado's ergonomic hand APIs is
> over-abstraction — it does NOT pay. Author for the actual shape; byte-pin
> every generated surface. Plan + rejection list: [`docs/MACRO-VOCABULARY.md`](./docs/MACRO-VOCABULARY.md).

> **★ M5 de-overlap with tear.** Mado's legacy `pane.rs` / `tab.rs` /
> `window.rs` have been **DELETED** — multiplexing lives in
> [`pleme-io/tear`](https://github.com/pleme-io/tear) (the
> tmux-compatible multiplexer), not here; mado is single-pane
> (`single_pane.rs`) and attaches to tear. The canonical destination
> and phased plan live in
> [`theory/MADO-TEAR-M5.md`](../theory/MADO-TEAR-M5.md). Phase 1
> (tear-daemon UDS RPC + tear-client) shipped at tear@0d0a240; Phase 2
> (tear-core gains per-pane vte parsing + cell grids) is the next
> heavy lift. **Do not re-introduce in-mado multiplexing** — pane/tab/
> window state belongs in tear-core, never back in mado.

> **★ Capability gap analysis + remediation plan (2026-05-31).** A full
> 13-agent audit of mado against the modern terminal landscape
> (Kitty/Ghostty/WezTerm/iTerm2/foot/rio/Contour/Windows Terminal/Warp/
> tmux/zellij) and the destination-first plan to close every gap live in
> [`docs/GAP-ANALYSIS.md`](./docs/GAP-ANALYSIS.md) (120-capability matrix +
> prioritized P0–P2 gaps, evidence-cited) and
> [`docs/REMEDIATION-PLAN.md`](./docs/REMEDIATION-PLAN.md) (8 milestones
> M0–M7, the one-touch Cell/Grid co-design, substrate-extraction map).
> **Headline:** the DEFAULT (embedded-tear) render mode currently lacks
> copy/paste/selection/search/mouse — they exist only in the legacy
> local-PTY path; M1 unifies both into one `mado::ux::InputEngine` so a
> capability proven once holds in every mode. M0 quick-wins (capability-
> honest TERM, shikumi dead-knob invariant, delete the Rhai/soushi stubs,
> paste-safety) gate nothing and ship from day one. Read these before
> adding terminal features.

> **★ Floating browser — "The DOM Way of the Browser" (M0/M1 shipped).** mado
> hosts N floating, snapping, GPU-composited browser surfaces, drivable
> identically over MCP (`browser_*`), tatara-lisp (`(mado-browser-*)`), and
> declarative layout (`(deffloatingbrowser)`/`(defsnapzone)`). The panel shows
> REAL page pixels: nami-core parses HTML → `DisplayList`, `render.rs`
> `draw_float_panels` rasterizes it per-surface (garasu offscreen → texture,
> cached by content seqno) + composites via the E1 per-quad-opacity image path.
> The DOM is a **tatara-lisp value**: nami-core `eval` is re-enabled →
> `inline_lisp::expand` macroexpands inline lisp in the page tree before paint
> (DOM manipulation as lisp; no-op for plain HTML) + `dom_to_sexp` is queryable
> via `(mado-browser-dom-sexp id)`. The pure geometry/snap/state core is in
> `src/float/` (39 tests); the engine + render port in `src/browser_engine.rs`;
> the control substrate in `src/browser_bridge.rs`. **Doctrine + phased M0..M4
> ledger + named debt (`pending-shared-content-translator`,
> `pending-browser-async-fetch`):**
> [`theory/DOM-WAY.md`](../theory/DOM-WAY.md). Do NOT re-author the DrawCmd→GPU
> translator a third time — the Op#1 fix is one shared crate.

## Build & Test

```bash
cargo build
cargo run
cargo test                    # ~1,406 #[test] + 65 #[tokio::test] + 12 proptest!
RUST_LOG=debug cargo run      # with tracing
nix build                     # Nix package
nix run .#rebuild             # rebuild HM module (from nix repo)
```

## Competitive Position

| vs | Mado advantage |
|----|----------------|
| **Ghostty** | embedded MCP automation (63 typed tools), tatara-lisp scripting via the embedded vigy reconciler (**default OFF** — `vigy.enabled = true` to use it), Nix-native typed config (shikumi) |
| **WezTerm** | wgpu not OpenGL, pure-safe Rust (no C deps), tatara-lisp scripting (vigy) not Lua, Nix-managed config via shikumi, MCP |
| **Kitty** | Modal vim-style hotkeys (awase), MCP + tatara-lisp scripting (vigy) instead of Python kittens |
| **Alacritty** | Embedded tear multiplexer (panes/tabs/sessions), MCP automation (Alacritty is intentionally minimal) |
| **Rio** | embedded MCP + vigy automation, deeper Nix integration |

## Architecture

### Data Flow

```
Shell --> PTY (openpty) --> async reader --> vte parser --> Terminal Grid
                                                             |
    GPU: clear --> RectPipeline (cell bg + cursor + decor) --> glyphon text
                                                             ^
Input Events --> madori (winit) --> event handler --> Terminal Grid / PTY writer
                                                             ^
Config <-- shikumi (hot-reload, ArcSwap) <-- ~/.config/mado/mado.yaml
```

### Source Modules

| Module | Lines | Purpose | Key Types |
|--------|-------|---------|-----------|
| `terminal.rs` | ~11300 | VT100/xterm state machine | `Terminal`, `Grid`, `Cell`, `CellAttrs`, `Color`, `MouseMode` |
| `render.rs` | ~9500 | Multi-pass GPU pipeline | `TerminalRenderer`, `RectPipeline`, `Snapshot` |
| `main.rs` | ~1450 | Event loop, input dispatch | CLI args, clipboard, double/triple click, single-pane wiring |
| `config.rs` | ~6200 | shikumi config with hot-reload | `MadoConfig`, `load_and_watch()` |
| `selection.rs` | ~390 | Mouse text selection | `Selection`, `CellPos` |
| `keybind.rs` | ~350 | Configurable keybindings | `KeybindManager`, `Action`, `Key` |
| `pty.rs` | ~330 | PTY allocation + async I/O | `Pty`, `PtyReader`, `PtyWriter` |
| `theme.rs` | ~280 | Color theme system | `Theme`, 8 built-in themes (Nord, Dracula, etc.) |
| `search.rs` | ~270 | Scrollback search | `SearchState`, `SearchMatch` |
| `url.rs` | ~180 | URL detection (no regex) | `DetectedUrl`, `detect_urls_in_row` |
| `platform.rs` | ~95 | Platform-native integration | Pure safe Rust via objc2 (macOS styling, dark mode, dock badge) |
| `module/default.nix` | | Home-manager module | `blackmatter.components.mado.*` |

### Threading Model

Current: two threads.
```
Main thread:    madori event loop --> winit --> GPU render (60fps)
PTY thread:     tokio runtime --> reader (PTY->Terminal) + writer (input->PTY) + resize
```

Target (Ghostty-inspired four-thread model):
```
Main thread:    Platform event loop, user input, config updates
I/O thread:     PTY writes, VT parsing, mailbox drain
Read thread:    Blocking PTY reads (avoids blocking I/O thread)
Render thread:  GPU rendering at native refresh rate, decoupled from I/O
```

The current two-thread model works but couples rendering to the main thread.
Separating rendering onto its own thread eliminates frame drops during heavy
I/O (e.g., `cat` of large files). The I/O/read thread split prevents PTY
write stalls from blocking parse progress.

### GPU Rendering Pipeline

Current: three sequential passes (back-to-front):

1. **Clear** -- Full-screen background fill (configurable color + opacity)
2. **RectPipeline** -- Instanced colored rectangles via custom WGSL shader:
   - Cell backgrounds (ANSI/256/truecolor)
   - Cursor (block/bar/underline with optional blink)
   - Underline and strikethrough decorations
   - Selection highlight (semi-transparent Nord frost overlay)
   - Box drawing / powerline sprites (14 box chars + 8 block elements)
   - Bell flash overlay (4-frame decay)
   - Search match highlights (current=yellow, others=dim)
   - URL underline (frost-blue for detected URLs)
3. **Text** -- Per-row glyphon buffers with per-cell color spans:
   - Bold-as-bright (ANSI 0-7 to 8-15 when bold)
   - Font family from config via `glyphon::Family::Name`
   - Bold weight, italic style per span

Target (six-pass model):
```
1. Background color       -- opaque fill, no blending
2. Cell backgrounds       -- per-cell RGBA with alpha compositing
3. Cell text              -- dual atlas (grayscale + color/emoji)
4. Images                 -- Kitty graphics protocol textured quads
5. Background images      -- user wallpapers with fit/positioning
6. Post-processing        -- custom WGSL shader chain
```

**Per-layer-isolated text** (`garasu::TextLayerStack`): the terminal grid, the
picker/overlay, and the search-status each draw on their OWN layer (own glyphon
vertex buffer + own `Viewport`) of one shared atlas, minted once by
`ensure_layers` (`TEXT_LAYERS` names them). `render()` opens ONE `Frame` across
Pass 3 + Pass 6 and drops it (trim-once) before submit. This makes the
top-left-blank class — a second text pass clobbering the first's recorded
glyphs — unrepresentable (§VIII #8). `pending-engawa-text:` the residual
cross-layer-eviction + intra-layer-double-prepare axes are *only-mitigated*
here; they close truly-unrepresentably only at the engawa destination
(`ResourceKind::TextLayer` + the shipped `MultipleWriters` validation), so this
`TextLayerStack` interim must never be enshrined as the end state.

Key GPU optimizations to implement:
- **Dual texture atlas**: Separate grayscale (regular glyphs) and BGRA (emoji/color)
  atlases for memory efficiency. Currently one shared glyphon atlas across all
  text layers (`TextLayerStack`).
- **Instanced rendering**: Already using instanced rects. Extend to text quads
  for elimination of per-row buffer creation overhead.
- **Damage tracking**: Already have sequence number tracking to skip unchanged
  frames. Extend to per-region dirty tracking.
- **Linear blending**: Use `*_srgb` render targets so GPU blends in linear
  color space (physically correct). Currently blending in sRGB.

### Terminal Emulation

**VT parser**: vte crate (state machine approach matching VT100.net spec).

**Grid**: `VecDeque<Vec<Cell>>` -- O(1) scroll via push_back/pop_front. Primary
and alternate screen buffers. Configurable scrollback (default 10,000 lines).

**Cell**: 5 fields, **24 bytes** -- `ch: char`, `extra: Option<Box<Vec<char>>>`
(combining), `width: u8` (0=continuation, 1=normal, 2=wide), `style_id: u16`,
`link_id: u16`. fg/bg/attrs are **interned**: `style_id` indexes a per-grid
`StyleTable`, `link_id` a `LinkTable`, so most cells share one style entry. A
`size_of::<Cell>() <= 24` guard is live in `terminal.rs`.

> **The Ghostty-style 24-byte + style-dedup cell has LANDED** — this is the
> shipped `Cell`, not a Phase-4 target. (Pack codepoint + style ID + flags:
> done via `style_id`. Dedup styles per page: done via `StyleTable`/`LinkTable`
> interning.) The one remaining sub-item is a grapheme side-table with a
> cell-held offset; today combining marks live in `extra: Option<Box<Vec<char>>>`.

**Implemented sequences**:
- Cursor: CUU/CUD/CUF/CUB/CUP/CHA/VPA/CNL/CPL, DECSC/DECRC
- Erase: ED/EL/ECH/DCH/ICH
- Scroll: SU/SD, IL/DL, DECSTBM scroll regions
- SGR: bold, dim, italic, underline, blink, inverse, hidden, strikethrough, 8/16/256/truecolor
- DEC modes: DECTCEM, DECAWM, DECOM, DECCKM
- Alternate screen: 47/1047/1049
- Mouse: modes 1000/1002/1003, X10 + SGR (1006) encoding
- Tab: HTS, CBT (CSI Z), TBC (CSI g)
- Reports: DA, secondary DA (CSI >c), DSR 5/6
- REP (CSI b), bracketed paste (2004), synchronized output (2026)
- Focus reporting (1004)
- OSC 0/2 (title), OSC 7 (CWD), OSC 52 (clipboard), OSC 8 (hyperlinks)
- OSC 4 (color palette query), OSC 10/11/12 (fg/bg/cursor color query)
- OSC 133 (semantic prompt marking A/B/C/D)
- DCS DECRQSS (request setting state: SGR, DECSTBM, DECSCL, DECSCA)
- Kitty keyboard protocol (push/pop/query stack, progressive enhancement)
- Kitty graphics protocol (inline PNG images, multi-chunk, placement, GPU upload)
- DEC Special Graphics charset (ESC ( 0, Shift In/Out)
- IRM insert mode, DECSTR soft reset, DECRQM mode queries
- DA3 tertiary device attributes, DECALN screen alignment test
- DECKPAM/DECKPNM keypad modes

**Missing sequences** (ordered by priority):
1. G2/G3 + LS2/LS3 and UTF-8 DOCS charset designation
2. Full blink-attribute rendering (partial)
3. Complete colour-emoji coverage: COLR/CBDT (partial)
4. DECOM origin-mode constraint (partial)

> **Corrected 2026-07-31.** SIXEL used to head this list and is **SHIPPED** —
> `DCS q` payload → `icy_sixel::DcsSettings` → `decode_and_place_sixel`
> (terminal.rs:4394-4403), sharing the same `store_rgba_image` texture path as
> Kitty graphics, bounded by `SIXEL_DCS_MAX` (8 MiB) with a poison-past-cap
> guard and two pinning tests. Items 1-4 come from docs/GAP-ANALYSIS.md
> (2026-05-31) and are **not re-verified since** — treat as likely-still-open,
> not confirmed. That doc's headline complaint (no copy/paste/select/search in
> the default mode) is CLOSED by the M1 `ux::InputEngine` unification; do not
> cite it as current.

### Font System

Current: glyphon (cosmic-text fork) handles font discovery, shaping, and
rasterization. Font family set per-span via `Attrs::family()`. System font
matching via cosmic-text's `FontSystem::new()`.

Target (three-layer architecture):
```
1. Discovery     -- platform font enumeration (CoreText/Fontconfig)
2. Shaping       -- HarfBuzz for ligature support, grapheme clusters
3. Rasterization -- glyph cache in GPU texture atlas
```

Key font features to implement:
- **Font fallback chain**: Multiple `font-family` entries with automatic system fallback
- **Ligatures**: HarfBuzz shaping with `-calt` control. Break ligatures under the cursor
- **Synthetic styles**: Skew transform for synthetic italic when font lacks italic face
- **Variable fonts**: Expose variation axes in config
- **Nerd Font embedding**: Ship bundled Nerd Font symbols for zero-config icons

### Input Handling

**Keyboard**:
- Text input forwarded directly to PTY
- Ctrl+letter to control byte (0x01..0x1A)
- Alt+key to ESC prefix + character
- Cursor keys: application mode (ESC O) vs normal (ESC [)
- F1-F12 escape sequences
- Cmd+C/V: clipboard copy/paste (via hasami)
- Bracketed paste wrapping when mode 2004 active
- Configurable keybindings via KeybindManager

**Mouse**:
- Single click: start drag selection
- Double click (400ms window): word selection (alphanumeric + underscore)
- Triple click: line selection
- Drag: update selection endpoint
- Scroll: routed through the **scroll system** (`src/ux/scroll.rs`) — one typed
  `ScrollSystem` that maps a source-typed gesture (`ScrollGesture::Wheel` ticks
  vs `ScrollGesture::Precise` trackpad pixels — kept distinct by madori's typed
  `ScrollDelta`) + live context to a typed `ScrollAction` (viewport scroll /
  forward wheel reports / alt-screen arrows). **Precise (trackpad) = ghostty's
  pixel accumulator** (peel whole cells via cell height, carry the signed
  sub-cell remainder, NO synthetic friction — the OS momentum-phase stream that
  winit forwards as more `PixelDelta`s supplies the inertia); **wheel = direct
  lines OR mado's synthetic momentum glide** (a deliberate superset ghostty
  lacks). A precise gesture cancels any in-flight wheel glide. O(1) offset
  clamp ⇒ a fast fling respects the `usize::MAX` infinite-scrollback default.
  Behaviors are selected by typed config (see Configuration); the engine is the
  pure I/O edge.
- Mouse forwarding: X10 and SGR encoding for modes 1000/1002/1003

**IME**: winit IME events forwarded -- Commit text goes to PTY.

**Focus**: `\x1b[I`/`\x1b[O` sent when focus reporting (mode 1004) enabled.

---

## Shared Library Integration

| Library | Used For |
|---------|----------|
| **garasu** | `GpuContext`, `TextRenderer`, shaders, `AppWindow` |
| **madori** | `App::builder()`, `RenderCallback`, `AppEvent`, `EventResponse` |
| **shikumi** | `ConfigDiscovery`, `ConfigStore<T>`, hot-reload |
| **hasami** | `Clipboard`, `ClipboardProvider` for copy/paste |

All deps via path references in Cargo.toml with `[patch]` sections to unify
transitive git deps.

> **NOT published to crates.io, and cannot be.** `Cargo.toml` carries
> `publish = false`: the `mado` name on crates.io is owned by another
> publisher, so a publish could never succeed. The remedy (rename the package
> to `pleme-io-mado`, keep `[[bin]] name = "mado"`) is a decision about the
> crate's public identity and is deliberately left open. Distribution is the
> DMG / Homebrew cask / `nix run` paths in the README, not crates.io.

### Libraries to integrate (not yet wired)

**Read the tier column before citing this table** — "not yet wired" spans
three genuinely different states, and they were previously indistinguishable.

| Library | Role in Mado | State (2026-07-31) |
|---------|-------------|--------------------|
| **egaku** | Tab bar, pane split handles, command palette, search overlay widgets | **PARTIAL — geometry only.** Consumed for `egaku::Rect` (17 refs); Cargo.toml says so explicitly. The widget chrome lands at QUADRO T1. |
| **irodori** | Color palette for themes (replace hardcoded Nord values) | not a dependency; zero call sites |
| **irodzuki** | GPU theming: base16 to wgpu uniforms, ANSI color table generation | wired |
| **kaname** | Embedded MCP server (stdio transport) | **DEAD DEP — declared at Cargo.toml:241, ZERO `kaname::` call sites.** It is compiled and linked and costs build time + closure for nothing. The MCP server is **rmcp 0.15 directly** (63 tools, src/mcp.rs). Either wire it or drop the dependency. |
| **awase** | Modal hotkey system (Normal/Insert/Command modes) | wired (`KeyRepeatGate`, keybinds) |
| **mojiban** | Rich text in command palette and help overlays | not a dependency; zero call sites |
| **tsunagu** | Daemon mode (background multiplexer with IPC) | **SUPERSEDED by tear.** Not a dependency. Multiplexing left mado at Phase 4; do not re-introduce this edge. |
| **tsuuchi** | Desktop notifications — native `UNUserNotificationCenter` backend (bundled), focus-aware center. See `docs/NOTIFICATIONS.md` | wired (backend select at platform.rs:169-192; center at notify_center.rs) |
| **todoku** | HTTP client for update checks, plugin registry | not a dependency; zero call sites |

---

## Configuration

- **File**: `~/.config/mado/mado.yaml`
- **Env override**: `MADO_CONFIG=/path/to/config.yaml`
- **Env prefix**: `MADO_` (e.g., `MADO_FONT_SIZE=16`)
- **Hot-reload**: shikumi `ConfigStore::load_and_watch` with symlink-aware
  file watcher (works with nix-darwin managed configs)
- **HM module**: `blackmatter.components.mado.*` generates YAML from typed Nix options

Config sections: `font_family`, `font_size`, `window` (width/height/padding),
`shell` (command), `cursor` (style/blink/blink_rate_ms), `behavior`
(scrollback_lines/copy_on_select), `appearance` (background/foreground/opacity).

Scroll knobs (under `behavior`, projected into the typed `ux::scroll::ScrollConfig`
via `UxBehavior::scroll_config`): `scroll_momentum` (wheel: Lines vs Momentum),
`mouse_scroll_multiplier` (wheel lines/notch), `scroll_friction` + `scroll_max_velocity`
(momentum-glide tuning), `precise_scroll_mode` (`pixels` = ghostty OS-inertia
accumulator | `momentum` = synthetic glide), `precise_scroll_multiplier` (trackpad
pixel gain; default 2.0 ≈ ghostty macOS feel), `selection_autoscroll` +
`selection_autoscroll_speed` + `selection_autoscroll_max_overshoot` (drag-past-edge
auto-scroll). Each knob is live + dead-knob-tested.

Target config features:
- **Theme system**: Named themes (Nord, Dracula, etc.) switchable at runtime -- 8 built-in themes done
- **Keybinding customization**: Key to action mapping -- done via KeybindManager
- **Per-profile configs**: Multiple named configurations -- done via MadoConfig.with_profile()
- **Automatic light/dark mode**: Switch themes based on system appearance

---

## MCP Server (rmcp)

Embedded MCP server via stdio transport, discoverable at `~/.config/mado/mcp.json`.

> **Corrected 2026-07-31: this is `rmcp` 0.15 directly, not kaname.** The
> section was titled "MCP Server (kaname)" and kaname has **zero call sites**
> in `src/` — see the dead-dep row in the shared-library table above.

**Standard tools**: `status`, `config_get`, `config_set`, `version`

**Terminal-specific tools**:
| Tool | Description |
|------|-------------|
| `list_sessions` | List all open terminal sessions (panes/tabs) |
| `send_keys` | Send keystrokes to a specific session |
| `get_output` | Get recent terminal output (last N lines) from a session |
| `create_split` | Create a new split pane (horizontal/vertical) |
| `run_command` | Run a command in a new or existing session |
| `get_terminal_state` | Get cursor position, dimensions, title, CWD |
| `set_font` | Change font family/size at runtime |
| `set_theme` | Switch color theme at runtime |
| `simulate_chord` | Resolve a chord (e.g. `cmd+g`) against the LIVE GUI's keybindings via kanshou and inject the bound Action into the GUI event loop (typed `InjectedActions` queue; `send_keys` only reaches the PTY) |

The `mado e2e` subcommand (docs/INTEGRATION-TESTING.md §L2) is the
typed rmcp client for this surface: it spawns `mado mcp` as a stdio
child and runs the smoke matrix (spawn_term → prompt visible → Enter
→ fresh prompt → `echo E2E_MARKER` round-trip), printing a JSON row
summary and exiting nonzero on any failure.

---

## Automation & scripting (MCP + vigy / tatara-lisp)

mado exposes two typed automation primitives — **no third scripting engine**:

- **rmcp MCP server** (**63** typed tools — session/grid snapshots, send-keys,
  clipboard history, prompt/command blocks, asciinema recording, 18 `tear_*`,
  11 `browser_*`, 5 `vigy_*`, 3 `suggest_*`; see the MCP Server section below)
  for agent / external drive.
- **embedded vigy reconciler** running **tatara-lisp** in-process (see
  `vigy_host.rs`). Scripting IS tatara-lisp: user automation is authored as
  `(def…)` forms over mado intrinsics registered through vigy's `register_fn`
  / `ExtInterpreter` (e.g. `(mado-tear-attached?)`). Output-driven triggers
  and a shell-facing `mado` control CLI land as a thin layer over this + the
  MCP surface (see [`docs/REMEDIATION-PLAN.md`](./docs/REMEDIATION-PLAN.md) M7).

**Scope boundary — mado-memory-privileged only.** Built-in tatara-lisp
scripting exists *only* for operations that require privileged access to
mado's live in-process state: the grid + scrollback, selection,
prompt/command blocks, clipboard history, pane/session graph,
cursor/mode/VT state, typed config, and the send-to-PTY / window / title /
theme / bell / font handles. The vigy intrinsics expose exactly this
"reach into mado's brain" surface — nothing more. If a task can be done by
running a command in a **shell, it belongs in the shell (frostmourne)**, not
in mado scripting; mado scripting is NOT a general automation/orchestration
language and must never reimplement shell-doable work (file ops, git,
process spawning, pipelines). The test for every proposed intrinsic: *"does
this need mado's live state to work?"* — yes → mado intrinsic; no →
frostmourne. This keeps the scripting surface small, typed, and
non-overlapping with the shell.

> A Rhai/soushi scripting stub previously lived here; it was **deleted
> (2026-05-31)** — the functions never had a real terminal handle (they
> returned format-string placeholders), so the API was doc/code drift and
> carried TYPED-EMISSION `format!()` violations. Per the Four-Lisps /
> solve-once discipline, scripting consolidates on **tatara-lisp via vigy**,
> not a second engine.

---

## Hotkey System (awase)

Modal vim-style keybindings:

| Mode | Purpose | Enter via |
|------|---------|-----------|
| **Normal** | Default mode, terminal passthrough | Automatic |
| **Command** | `:` prefix commands, command palette | `:` key |
| **Search** | `/` forward search, `?` backward | `/` or `?` key |
| **Visual** | Text selection mode | `v` key (when not in PTY) |

Configurable in `~/.config/mado/mado.yaml` under `keybindings:`. Platform-aware:
Cmd on macOS, Ctrl on Linux.

---

## Shader Plugins

Custom WGSL shaders in `~/.config/mado/shaders/*.wgsl`:

- Input bindings: `input_texture` (binding 0), `input_sampler` (binding 1),
  `uniforms` (binding 2: time, resolution)
- Post-processing chain: shaders applied in filename order after main render
- Built-in accessibility shaders: protanopia, deuteranopia, tritanopia
  (Machado 2009 color vision simulation matrices)

---

## Shell Integration

Shell scripts in `shell-integration/`:
- `mado.bash`, `mado.zsh`, `mado.fish`
- Emit OSC 133 (prompt marking), OSC 7 (CWD reporting), OSC 2 (title)
- Installed automatically via HM module

### Emitting terminal escapes — use `src/vt.rs`, never raw bytes

Every outbound control sequence mado writes (PTY replies, the
`notify-test` OSC demo, anything with dynamic params) is built through the
typed `vt` emitters — `csi` / `dcs` / `apc` / `osc` and the notification
helpers `osc9_notify` / `osc777_notify` / `osc99_notify` /
`osc1337_request_attention`. Declare the code + typed params, never
hand-spell `\x1b]…` or `format!()` a sequence (★★ TYPED EMISSION). Each
builder is byte-pinned by a test. A fixed constant with no composition
(e.g. `b"\x1b[I"` focus report) is fine; anything embedding a value goes
through `vt`.

---

## Roadmap

### Phase 1 -- Core Correctness [DONE]
All VT100/xterm sequences, mouse tracking, Kitty keyboard/graphics protocols,
DCS/DECRQSS, OSC 52/8/133/4/10/11/12, shell integration.

### Phase 2 -- Rendering Quality [IN PROGRESS]
Dual texture atlas, HarfBuzz shaping, font fallback, synthetic italic,
sRGB-correct linear blending, subpixel text, custom shader chain.

### Phase 3 -- Features [DONE]
Themes, keybindings, search, URL detection, bell, Kitty graphics, sixel,
Kitty keyboard, shell integration, profile system.

> **Corrected 2026-07-31: "Split panes, tabs" used to head this DONE list and
> is FALSE — they were DELETED, not shipped.** Commits `bdb8721` + `f26bb00`
> ("Phase 4") removed `pane.rs`/`tab.rs`/`window.rs`, `render_multi_pane` and
> `snapshot_pane`, because tear already owned sessions/windows/panes/layouts
> correctly and mado's copy was duplication that drifted. **mado is
> single-pane by design.** Multi-pane returns by rendering tear's
> `LayoutNode`/`compute_rects` — tracked as M5 in
> `tear/docs/SESSION-TYPESCAPE.md`, and it is NOT a restore: there is no
> clipping primitive anywhere in the GPU stack to turn on (measured: zero
> `set_scissor_rect`, zero `set_viewport` across mado/garasu/engawa/madori).

### Phase 4 -- Architecture [NEXT]
Four-thread model, paged memory (mmap, CoW, style dedup), terminal inspector,
Quick Terminal, native menus.

> Two items were struck 2026-07-31: **daemon mode (tsunagu)** is SUPERSEDED by
> tear — do not re-introduce that edge — and the **MCP server SHIPPED** (rmcp,
> 63 tools), so it is not a future phase. The four-thread model remains a
> genuine target; today the VT/render data path is two threads, and
> `src/grid_damage.rs`'s `DirtyRegion` types are built but deliberately
> unwired pending it.

### Phase 5 -- Polish
Variable fonts, Nerd Font embedding, vttest full pass, Ghostty-level throughput,
accessibility (contrast enforcement, font scaling, reduce motion).

---

## Design Decisions

### Why madori (not raw winit)?
Madori provides the event loop -> GPU init -> render loop -> input dispatch
scaffold. Every GPU app (mado, hibiki, kagi, etc.) shares this ~200-line
boilerplate. Madori owns the window; mado implements `RenderCallback` and
receives `AppEvent`s.

### Why vte (not custom parser)?
vte is battle-tested (used by Alacritty) and handles the full VT state
machine including DCS/OSC/APC correctly. Writing a custom parser is a
multi-month effort with diminishing returns. If we need to extend it,
vte's `Perform` trait makes it straightforward.

### Why VecDeque grid (not paged memory)?
VecDeque gives O(1) scroll and is simple to implement correctly. Ghostty's
paged memory (mmap + CoW + style dedup) is superior for memory efficiency
at scale (millions of scrollback lines) but adds significant complexity.
We'll migrate to paged memory in Phase 4 after the emulation layer is
proven correct.

### Why garasu (not raw wgpu)?
garasu provides `GpuContext` (device/queue/adapter), `TextRenderer` (glyphon),
and `ShaderPipeline` (WGSL post-processing) as reusable primitives shared
across all pleme-io GPU apps. Mado uses garasu's text renderer for glyphon
integration and will use `ShaderPipeline` for custom shader effects.

### Bold-as-bright
Traditional terminals brighten ANSI colors 0-7 to 8-15 when bold. We
implement this at render time via `bold_bright_color()` which compares
the cell's fg RGB against the ANSI palette. Modern programs using 256/
truecolor are unaffected since their colors won't match the standard
palette entries.

### Pure safe Rust for macOS platform integration
`platform.rs` uses objc2 for all macOS Cocoa API calls -- zero `unsafe` blocks.
This includes transparent titlebar, dark mode detection, and dock badge updates.

---

## Nix Integration

- **Flake**: `packages.aarch64-darwin.default`, `overlays.default`, `homeManagerModules.default`
- **HM module path**: `module/default.nix` using substrate `hm-service-helpers.nix`
- **Build**: `pkgs.rustPlatform.buildRustPackage` (not yet using substrate `rust-tool-release-flake.nix` -- migrate when stabilized)
- **Config management**: HM module generates `~/.config/mado/mado.yaml` from typed Nix options
- **Shell integration**: HM module installs shell scripts to `~/.config/mado/shell-integration/`
