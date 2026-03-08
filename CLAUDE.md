# Mado (窓) — GPU-Rendered Terminal Emulator

Mado follows Ghostty's philosophy: **speed + features + native UI** without
compromise. Pure Rust, GPU-accelerated via wgpu (Metal on macOS, Vulkan on
Linux), zero-config defaults with deep configurability.

## Build & Test

```bash
cargo build
cargo run
cargo test              # 114 tests
RUST_LOG=debug cargo run  # with tracing
```

## Architecture

### Data Flow

```
Shell → PTY (openpty) → async reader → vte parser → Terminal Grid
                                                        ↓
    GPU: clear → RectPipeline (cell bg + cursor + decorations) → glyphon text
                                                        ↑
Input Events → madori (winit) → event handler → Terminal Grid / PTY writer
                                                        ↑
Config ← shikumi (hot-reload, ArcSwap) ← ~/.config/mado/mado.yaml
```

### Modules

| Module | Purpose | Key Types |
|--------|---------|-----------|
| `terminal.rs` | VT100/xterm state machine | `Terminal`, `Grid`, `Cell`, `CellAttrs`, `Color`, `MouseMode` |
| `render.rs` | Three-pass GPU pipeline | `TerminalRenderer`, `RectPipeline`, `Snapshot` |
| `pty.rs` | PTY allocation + async I/O | `Pty`, `PtyReader`, `PtyWriter` |
| `config.rs` | shikumi config with hot-reload | `MadoConfig`, `load_and_watch()` |
| `selection.rs` | Mouse text selection | `Selection`, `CellPos` |
| `search.rs` | Scrollback search | `SearchState`, `SearchMatch` |
| `url.rs` | URL detection (no regex) | `DetectedUrl`, `detect_urls_in_row` |
| `theme.rs` | Color theme system | `Theme`, 8 built-in themes |
| `keybind.rs` | Configurable keybindings | `KeybindManager`, `Action`, `Key` |
| `tab.rs` | Tab management | `TabManager`, `Tab`, `TabId` |
| `pane.rs` | Split pane layout | `PaneManager`, `PaneNode`, `SplitDir` |
| `window.rs` | Multi-pane/tab state | `WindowState`, `PaneTerminal` |
| `main.rs` | Event loop, input dispatch | CLI, clipboard, double/triple click |
| `platform.rs` | Platform-native integration | Pure safe Rust via objc2 (macOS styling, dark mode, dock badge) |
| `module/default.nix` | Home-manager module | `blackmatter.components.mado.*` |

### Threading Model

Current: two threads.
```
Main thread:    madori event loop → winit → GPU render (60fps)
PTY thread:     tokio runtime → reader (PTY→Terminal) + writer (input→PTY) + resize
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

Three sequential passes (back-to-front):

1. **Clear** — Full-screen background fill (configurable color + opacity)
2. **RectPipeline** — Instanced colored rectangles via custom WGSL shader:
   - Cell backgrounds (ANSI/256/truecolor)
   - Cursor (block/bar/underline with optional blink)
   - Underline and strikethrough decorations
   - Selection highlight (semi-transparent Nord frost overlay)
3. **Text** — Per-row glyphon buffers with per-cell color spans:
   - Bold-as-bright (ANSI 0-7 → 8-15 when bold)
   - Font family from config via `glyphon::Family::Name`
   - Bold weight, italic style per span

Target (Ghostty-inspired six-pass model):
```
1. Background color       — opaque fill, no blending
2. Cell backgrounds       — per-cell RGBA with alpha compositing
3. Cell text              — dual atlas (grayscale + color/emoji)
4. Images                 — Kitty graphics protocol textured quads
5. Background images      — user wallpapers with fit/positioning
6. Post-processing        — custom WGSL shader chain
```

Key GPU optimizations to implement:
- **Dual texture atlas**: Separate grayscale (regular glyphs) and BGRA (emoji/color)
  atlases for memory efficiency. Currently using single glyphon atlas.
- **Instanced rendering**: Already using instanced rects. Extend to text quads
  for elimination of per-row buffer creation overhead.
- **Damage tracking**: Already have sequence number tracking to skip unchanged
  frames. Extend to per-region dirty tracking.
- **Linear blending**: Use `*_srgb` render targets so GPU blends in linear
  color space (physically correct). Currently blending in sRGB.

### Terminal Emulation

**VT parser**: vte crate (state machine approach matching VT100.net spec).

**Grid**: `VecDeque<Vec<Cell>>` — O(1) scroll via push_back/pop_front. Primary
and alternate screen buffers. Configurable scrollback (default 10,000 lines).

**Cell**: 6 fields — `ch: char`, `extra: Option<Box<Vec<char>>>` (combining),
`width: u8` (0=continuation, 1=normal, 2=wide), `fg/bg: Color`, `attrs: CellAttrs`.

Target cell optimization (Ghostty uses 24 bytes per cell with style dedup):
- Pack codepoint + style ID + flags into a fixed-size struct
- Deduplicate styles per page (most cells share the same style)
- Store grapheme clusters in a side table, cell holds offset

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
- OSC 0/2 (title), OSC 7 (CWD)
- ESC: RIS, IND, NEL, RI
- Unicode: wide chars (CJK), combining characters

**Recently added sequences**:
- OSC 52 — clipboard get/set (base64 encoded)
- OSC 8 — hyperlinks (URI stored in Cell.hyperlink)
- OSC 4 — query color palette entries
- OSC 10/11/12 — query fg/bg/cursor colors
- OSC 133 — semantic prompt marking (A/B/C/D markers)
- DCS DECRQSS — request setting state (SGR, DECSTBM, DECSCL, DECSCA)
- Kitty keyboard protocol — push/pop/query stack, progressive enhancement
- Kitty graphics protocol — inline PNG images via APC, multi-chunk, GPU upload
- DEC Special Graphics charset (ESC ( 0, Shift In/Out)
- IRM insert mode (ANSI mode 4)
- DECSTR soft reset (CSI ! p)
- DECRQM mode queries (ANSI + DEC private)
- DA3 tertiary device attributes
- DECALN screen alignment test (ESC # 8)
- DECKPAM/DECKPNM keypad modes

**Missing sequences** (ordered by priority):
1. **SIXEL** — legacy inline image protocol

### Font System

Current: glyphon (cosmic-text fork) handles font discovery, shaping, and
rasterization. Font family set per-span via `Attrs::family()`. System font
matching via cosmic-text's `FontSystem::new()`.

Target (Ghostty-inspired three-layer architecture):
```
1. Discovery     — platform font enumeration (CoreText/Fontconfig)
2. Shaping       — HarfBuzz for ligature support, grapheme clusters
3. Rasterization — glyph cache in GPU texture atlas
```

Key font features to implement:
- **Font fallback chain**: Multiple `font-family` entries with automatic
  system fallback when configured fonts lack a glyph
- **Ligatures**: HarfBuzz shaping with `-calt` control. Break ligatures
  under the cursor so individual chars remain visible.
- **Synthetic styles**: Skew transform for synthetic italic when font lacks
  italic face
- **Variable fonts**: Expose variation axes in config
- **Nerd Font embedding**: Ship bundled Nerd Font symbols for zero-config icons

### Configuration

- **File**: `~/.config/mado/mado.yaml`
- **Env override**: `MADO_CONFIG=/path/to/config.yaml`
- **Env prefix**: `MADO_` (e.g., `MADO_FONT_SIZE=16`)
- **Hot-reload**: shikumi `ConfigStore::load_and_watch` with symlink-aware
  file watcher (works with nix-darwin managed configs)
- **HM module**: `blackmatter.components.mado.*` generates YAML from typed
  Nix options

Config sections: `font_family`, `font_size`, `window` (width/height/padding),
`shell` (command), `cursor` (style/blink/blink_rate_ms), `behavior`
(scrollback_lines/copy_on_select), `appearance` (background/foreground/opacity).

Target config features:
- **Theme system**: Named themes (Nord, Dracula, etc.) switchable at runtime
- **Keybinding customization**: Configurable key → action mapping
- **Per-profile configs**: Multiple named configurations
- **Automatic light/dark mode**: Switch themes based on system appearance

### Shared Library Dependencies

| Library | Used For |
|---------|----------|
| **garasu** | `GpuContext`, `TextRenderer`, shaders |
| **madori** | `App::builder()`, `RenderCallback`, `AppEvent`, `EventResponse` |
| **shikumi** | `ConfigDiscovery`, `ConfigStore<T>`, hot-reload |
| **hasami** | `Clipboard`, `ClipboardProvider` for copy/paste |

All deps via path references in Cargo.toml with `[patch]` sections to unify
transitive git deps. No crates.io publishing yet — names not reserved.

### Input Handling

**Keyboard**:
- Text input forwarded directly to PTY
- Ctrl+letter → control byte (0x01..0x1A)
- Alt+key → ESC prefix + character
- Cursor keys: application mode (ESC O) vs normal (ESC [)
- F1-F12 escape sequences
- Cmd+C/V: clipboard copy/paste (via hasami)
- Bracketed paste wrapping when mode 2004 active

**Mouse**:
- Single click: start drag selection
- Double click (400ms window): word selection (alphanumeric + underscore)
- Triple click: line selection
- Drag: update selection endpoint
- Scroll: viewport scroll (scrollback) or forwarded to PTY when mouse tracking active
- Mouse forwarding: X10 and SGR encoding for modes 1000/1002/1003

**IME**: winit IME events forwarded — Commit text goes to PTY.

**Focus**: `\x1b[I`/`\x1b[O` sent when focus reporting (mode 1004) enabled.

## Ghostty Parity Roadmap

Organized by impact and dependency order.

### Phase 1 — Core Correctness
- [x] VT100/xterm base sequences (CUU/CUD/CUP/ED/EL/SGR/etc.)
- [x] Alternate screen buffer
- [x] Mouse tracking (modes 1000/1002/1003 + SGR)
- [x] Bracketed paste, synchronized output
- [x] Bold-as-bright color substitution
- [x] Double/triple click selection
- [x] Tab manipulation (CBT/TBC)
- [x] Config-driven appearance (bg/fg/opacity/font_family)
- [x] OSC 52 clipboard (base64 decode, clipboard_content field)
- [x] OSC 8 hyperlinks (Cell.hyperlink field, active_hyperlink tracking)
- [x] OSC 133 shell integration (prompt_start_row tracking)
- [x] OSC 4/10/11/12 color queries (respond with current palette)
- [x] Bell tracking (bell_pending flag with take_bell())
- [x] DEC Special Graphics charset (ESC ( 0 / ESC ) 0, full VT100 line drawing)
- [x] IRM insert mode (ANSI mode 4), DECSTR soft reset (CSI ! p)
- [x] DECRQM mode queries (ANSI CSI $ p, DEC private CSI ? $ p)
- [x] DA3 tertiary device attributes (CSI = c)
- [x] DCS DECRQSS (request setting state for SGR, DECSTBM, DECSCL, DECSCA)
- [x] DECALN screen alignment test (ESC # 8)
- [x] DECKPAM/DECKPNM keypad modes (ESC = / ESC >)
- [x] Shift In/Shift Out (SI/SO) for G0/G1 charset switching
- [ ] Full xterm compatibility audit (test against vttest)

### Phase 2 — Rendering Quality
- [ ] Dual texture atlas (grayscale + color/emoji)
- [ ] HarfBuzz font shaping (ligatures)
- [ ] Font fallback chain with size adjustment
- [ ] Synthetic italic (skew transform)
- [x] Box drawing / powerline sprite renderer (14 box chars + 8 block elements via rect pipeline)
- [ ] sRGB-correct linear blending
- [ ] Subpixel text rendering (CoreText on macOS)
- [x] Post-processing pipeline (offscreen → shader → surface blit)
- [ ] Custom WGSL shader chain from config files

### Phase 3 — Features
- [x] Split panes — state machine (pane.rs: PaneManager, binary tree layout)
- [x] Tabs — state machine (tab.rs: TabManager, add/close/navigate)
- [x] Theme system — 8 built-in themes (theme.rs: Nord, Dracula, etc.)
- [x] Configurable keybindings — state machine (keybind.rs: KeybindManager)
- [x] Search in scrollback — state machine (search.rs: SearchState)
- [x] URL detection — state machine (url.rs: DetectedUrl, no-regex)
- [x] Wire search/URL/keybind into render pipeline and event loop
- [x] Bell notification (visual flash overlay, 4-frame decay)
- [x] OSC 52 clipboard sync (terminal → system clipboard on redraw)
- [x] Search highlight rendering (current match yellow, other matches dim)
- [x] URL underline rendering (detected URLs get frost-blue underline)
- [x] Keybind-driven action dispatch (replaces hardcoded Cmd+C/V)
- [x] Wire pane/tab into main.rs with per-pane terminal+PTY
- [x] Kitty graphics protocol (inline PNG images, multi-chunk, placement, GPU texture upload)
- [x] Kitty keyboard protocol (push/pop/query stack, progressive enhancement)

### Phase 4 — Architecture
- [ ] Four-thread model (main/IO/read/render)
- [ ] Paged memory for scrollback (mmap, CoW, style dedup)
- [ ] Terminal inspector (live mode/cell/font debugging)
- [ ] Daemon mode via tsunagu (multiplexer)
- [x] Platform-native integration — macOS: transparent titlebar, dark mode, dock badge (pure safe Rust via objc2)
- [ ] Platform-native integration — macOS: Quick Terminal, Secure Keyboard Entry, native menus
- [ ] Platform-native integration — Linux: Wayland-native, desktop entry, notifications

### Phase 5 — Polish
- [ ] Variable font support
- [ ] Nerd Font embedding
- [x] Shell integration scripts (mado.bash/mado.zsh/mado.fish — OSC 133/7/2)
- [x] Profile system (MadoConfig.with_profile() merges named profile overrides)
- [ ] Performance: match Ghostty's 4x-over-iTerm throughput target
- [ ] vttest full pass
- [x] Accessibility colorblind shaders (protanopia, deuteranopia, tritanopia via Machado 2009 matrices)
- [ ] Accessibility contrast enforcement, font scaling, reduce motion

## Design Decisions

### Why madori (not raw winit)?
Madori provides the event loop → GPU init → render loop → input dispatch
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
