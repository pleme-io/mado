# Mado (窓) — GPU-Rendered Terminal Emulator

A GPU-accelerated terminal emulator built in pure Rust. Follows Ghostty's philosophy
of speed + features + native UI without compromise, but adds Rhai scripting, an
embedded MCP server, and deep Nix integration that no competitor offers.

## Build & Test

```bash
cargo build
cargo run
cargo test                    # 114+ tests
RUST_LOG=debug cargo run      # with tracing
nix build                     # Nix package
nix run .#rebuild             # rebuild HM module (from nix repo)
```

## Competitive Position

| vs | Mado advantage |
|----|----------------|
| **Ghostty** | Rhai scripting (Ghostty has none), embedded MCP server, plugin ecosystem, Nix-native config |
| **WezTerm** | wgpu not OpenGL, pure Rust (no C deps), Nix-managed config via shikumi, MCP |
| **Kitty** | Modal vim-style hotkeys (awase), MCP, Rhai scripting instead of Python kittens |
| **Alacritty** | Split panes, tabs, scripting, MCP, plugins (Alacritty is intentionally minimal) |
| **Rio** | Rhai not WASM, deeper Nix integration, MCP automation |

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
| `terminal.rs` | ~3300 | VT100/xterm state machine | `Terminal`, `Grid`, `Cell`, `CellAttrs`, `Color`, `MouseMode` |
| `render.rs` | ~2350 | Three-pass GPU pipeline | `TerminalRenderer`, `RectPipeline`, `Snapshot` |
| `main.rs` | ~1000 | Event loop, input dispatch | CLI args, clipboard, double/triple click, pane/tab wiring |
| `selection.rs` | ~390 | Mouse text selection | `Selection`, `CellPos` |
| `config.rs` | ~380 | shikumi config with hot-reload | `MadoConfig`, `load_and_watch()` |
| `window.rs` | ~380 | Multi-pane/tab state | `WindowState`, `PaneTerminal` |
| `keybind.rs` | ~350 | Configurable keybindings | `KeybindManager`, `Action`, `Key` |
| `pane.rs` | ~340 | Split pane layout | `PaneManager`, `PaneNode`, `SplitDir` |
| `pty.rs` | ~330 | PTY allocation + async I/O | `Pty`, `PtyReader`, `PtyWriter` |
| `theme.rs` | ~280 | Color theme system | `Theme`, 8 built-in themes (Nord, Dracula, etc.) |
| `search.rs` | ~270 | Scrollback search | `SearchState`, `SearchMatch` |
| `tab.rs` | ~220 | Tab management | `TabManager`, `Tab`, `TabId` |
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

**Grid**: `VecDeque<Vec<Cell>>` -- O(1) scroll via push_back/pop_front. Primary
and alternate screen buffers. Configurable scrollback (default 10,000 lines).

**Cell**: 6 fields -- `ch: char`, `extra: Option<Box<Vec<char>>>` (combining),
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
1. SIXEL -- legacy inline image protocol

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
- Scroll: viewport scroll (scrollback) or forwarded to PTY when mouse tracking active
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
transitive git deps. Published to crates.io as `mado`.

### Libraries to integrate (not yet wired)

| Library | Role in Mado |
|---------|-------------|
| **egaku** | Tab bar, pane split handles, command palette, search overlay widgets |
| **irodori** | Color palette for themes (replace hardcoded Nord values) |
| **irodzuki** | GPU theming: base16 to wgpu uniforms, ANSI color table generation |
| **kaname** | Embedded MCP server (stdio transport) |
| **soushi** | Rhai scripting engine for user plugins |
| **awase** | Modal hotkey system (Normal/Insert/Command modes) |
| **mojiban** | Rich text in command palette and help overlays |
| **tsunagu** | Daemon mode (background multiplexer with IPC) |
| **tsuuchi** | Desktop notifications for bell, background process completion |
| **todoku** | HTTP client for update checks, plugin registry |

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

Target config features:
- **Theme system**: Named themes (Nord, Dracula, etc.) switchable at runtime -- 8 built-in themes done
- **Keybinding customization**: Key to action mapping -- done via KeybindManager
- **Per-profile configs**: Multiple named configurations -- done via MadoConfig.with_profile()
- **Automatic light/dark mode**: Switch themes based on system appearance

---

## MCP Server (kaname)

Embedded MCP server via stdio transport, discoverable at `~/.config/mado/mcp.json`.

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

---

## Plugin System (soushi + Rhai)

Scripts loaded from `~/.config/mado/scripts/*.rhai`.

**Rhai API**:
```
mado.send(text)              // send text to active PTY
mado.split(direction)        // "horizontal" or "vertical"
mado.tab_new()               // open new tab
mado.tab_switch(n)           // switch to tab n
mado.set_opacity(f)          // set window opacity (0.0-1.0)
mado.shader_enable(name)     // enable a WGSL shader from shaders/
mado.font_size(n)            // set font size
mado.theme(name)             // switch theme
mado.title()                 // get current terminal title
mado.cwd()                   // get current working directory
mado.scrollback(n)           // get last n lines of scrollback
mado.selection()             // get current text selection
```

**Event hooks**: `on_startup`, `on_shutdown`, `on_bell`, `on_title_change`,
`on_directory_change`, `on_output(pattern)`, `on_focus`, `on_blur`

**Custom commands**: Plugins register commands accessible via command palette (`:` mode).

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

---

## Roadmap

### Phase 1 -- Core Correctness [DONE]
All VT100/xterm sequences, mouse tracking, Kitty keyboard/graphics protocols,
DCS/DECRQSS, OSC 52/8/133/4/10/11/12, shell integration.

### Phase 2 -- Rendering Quality [IN PROGRESS]
Dual texture atlas, HarfBuzz shaping, font fallback, synthetic italic,
sRGB-correct linear blending, subpixel text, custom shader chain.

### Phase 3 -- Features [DONE]
Split panes, tabs, themes, keybindings, search, URL detection, bell,
Kitty graphics, Kitty keyboard, shell integration, profile system.

### Phase 4 -- Architecture [NEXT]
Four-thread model, paged memory (mmap, CoW, style dedup), terminal inspector,
daemon mode (tsunagu), MCP server (kaname), Quick Terminal, native menus.

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
