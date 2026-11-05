# Mado (窓)

A GPU-rendered terminal emulator in Rust. Mado follows Ghostty's idea: speed,
features and a native feel, with no trade between them. It adds three things no
other terminal has: an embedded MCP server, an embedded blue/vigy runtime, and
typed configuration that Nix manages.

Version 0.1.220 (2026-11-04). macOS (Apple Silicon) is the main platform. Linux
builds too.

## What mado does

**Terminal core**

- VT100/xterm emulation on the `vte` parser: cursor, erase, scroll regions,
  SGR with 8/16/256/truecolor, DEC modes, alternate screen, mouse modes
  1000/1002/1003 with X10 and SGR encoding, bracketed paste, synchronized
  output, focus reports.
- OSC 0/2 title, 4 and 104 palette, 7 cwd, 8 hyperlinks, 10/11/12 colours,
  52 clipboard, 133 prompt marks, 1337 marks, 9/777/99 notifications.
- Kitty keyboard protocol, Kitty graphics, and SIXEL images (both share one
  GPU texture path with a byte cap).
- A 24-byte cell with interned styles and links. Scrollback is bounded by
  bytes, not by lines.

**Rendering**

- wgpu (Metal on macOS, Vulkan on Linux) through garasu. Instanced rects for
  cell backgrounds, cursor, decorations, selection and box drawing. glyphon text
  with HarfBuzz-class shaping (rustybuzz) on isolated text layers.
- Linear (sRGB-correct) blending. Paints only when a pixel changes: zero
  redraws at idle.
- Glass: a translucent window with full-strength text, rounded corners, a rim,
  an optional window band that mado draws itself, and optional frost
  ([`docs/GLASS.md`](./docs/GLASS.md)).
- WGSL post-processing shaders from `~/.config/mado/shaders/`, with built-in
  colour-vision simulation shaders.
- Calm motion from ishou motion tokens; the OS reduce-motion setting is
  followed ([`docs/MOTION.md`](./docs/MOTION.md)).

**Sessions (tear)**

- Mado is single-pane. Sessions, windows and panes live in
  [tear](https://github.com/pleme-io/tear), the tmux-compatible multiplexer.
- By default tear runs in-process (`tear.runtime: embedded`). `daemon` and
  `resident` share sessions across windows and processes. `tear.mode: never`
  uses a local PTY.
- Ctrl-S opens the session picker: switch, create, rename and kill sessions,
  with Home/End/PageUp/PageDown jumps.
- A typed recovery overlay offers actions when a session or pane faults.

**Everyday use**

- Selection (drag, double-click word, triple-click line), copy-on-select,
  Cmd-C/Cmd-V, safe bracketed paste, drag-and-drop of files and images.
- Scrollback search with highlights.
- Links: Cmd-hover and Cmd-click open URLs, OSC 8 links, and file paths with
  `path:line:col`. Files open in the editor you choose.
- Trackpad scroll with OS inertia; wheel scroll with optional momentum.
- Native macOS notifications (`UNUserNotificationCenter`), focus-aware, with
  pane alerts and command-finished cards.
- Prompt marks and command blocks from OSC 133, with an exit-status accent.
- Quick terminal, configurable keybindings, font zoom, profiles, themes
  (irodzuki presets plus the default `vellum`).
- Floating browser panels that render real page pixels (nami-core).
- The Ctrl-S board also shows a ranked suggestion stream: PRs, tickets,
  alerts and other tasks you can open as a named session
  ([`docs/SUGGESTION-STREAM.md`](./docs/SUGGESTION-STREAM.md)).

**Automation**

- `mado mcp`: an MCP server (rmcp, stdio) with 63 typed tools. Sessions,
  keys, grid snapshots, clipboard history, prompt blocks, asciinema
  recording, frame performance, 18 `tear_*`, 11 `browser_*`, 5 `vigy_*` and
  3 `suggest_*` tools.
- Blue: `~/.config/mado/mado.b` sets look, motion, font, any config group,
  keybindings and small commands. `~/.config/mado/vigies/*.b` starts blue
  vigies (reconcilers). Blue runs in a child process with a budget and a
  deadline, never on the render loop
  ([`docs/BLUE-WORLD.md`](./docs/BLUE-WORLD.md)).
- The scripting scope is mado's own live state only. Work that a shell can do
  stays in the shell.

## Architecture

```
Shell ── PTY ── tear-core (embedded) ── VT parse ── Grid
                                                     │
GPU: clear ── rects (bg, cursor, decor) ── text layers ── shaders
                                                     ▲
madori (winit) events ── ux::InputEngine ── Grid / PTY
                                                     ▲
shikumi config (YAML, hot-reload) + blue world (mado.b)
```

| Module | Purpose |
|---|---|
| `terminal` | VT state machine, grid, cells |
| `render` | GPU pipeline, snapshots, overlays |
| `gui_tear_attach`, `single_pane` | tear attach and the single-pane window |
| `ux` | one input engine for every runtime: keys, mouse, scroll, config apply |
| `config` | the typed `MadoConfig`, shikumi hot-reload |
| `world`, `vigy_host` | the blue world file and the embedded vigy runtime |
| `mcp` | the MCP server |
| `session_picker`, `suggest`, `safra` | the Ctrl-S board and its suggestion stream |
| `links`, `search`, `selection`, `clipboard_store` | everyday text tools |
| `notify_center`, `pane_alert`, `focus_server` | notifications |
| `float`, `browser_*` | floating browser panels |
| `glass`, `window_chrome` | translucent window and the drawn band |
| `platform` | macOS integration through objc2 |

## Libraries

| Library | Use |
|---|---|
| garasu | GPU context, text layers, pane scissor, shaders |
| madori | window, event loop, frame pacing |
| shikumi | typed config, discovery, hot-reload |
| tear | sessions, panes, PTY (embedded or daemon) |
| hasami | clipboard |
| awase | keybindings and key repeat |
| irodzuki | theme presets and ANSI tables |
| tsuuchi | desktop notifications |
| ishou | design and motion tokens |
| egaku | geometry |
| nami-core | HTML to display list for browser panels |
| blue-lang-host, vigy | blue world file and vigies |

## Build and test

```bash
cargo build
cargo run
cargo test                  # ~2,000 #[test], 67 #[tokio::test], 9 proptest!
cargo fmt --check           # CI gate
RUST_LOG=debug cargo run
nix build                   # substrate rust.tool package
```

Useful subcommands:

| Command | Does |
|---|---|
| `mado mcp` | MCP server on stdio |
| `mado config-show <tier>` | materialized config at a tier |
| `mado config-schema` | the config schema as JSON (feeds the Nix options) |
| `mado print-posture` | displays, refresh rates, GPU, platform as JSON |
| `mado e2e` | end-to-end smoke matrix through the MCP server |
| `mado record` / `mado scenario-run` | capture and replay a regression scenario |
| `mado bench rows` / `mado bench present` | pacing rows and per-frame paint/GPU time |
| `mado notify-test` / `mado feedback-test` | sample notification and OSC 133 escapes |
| `mado banner` | the wordmark and version |

## Install

Three easy macOS paths (open source → frictionless), plus Linux + Nix.
All channels build the macOS `.app` / Linux `.desktop` from the same
icon (`assets/mado-icon.svg`).

> **TL;DR (macOS):** grab the always-fresh DMG from
> [**releases/latest**](https://github.com/pleme-io/mado/releases/latest)
> and drag to Applications, **or** `brew install --cask`, **or**
> `nix run github:pleme-io/mado`.

Releases come in two streams:

- **Rolling latest** — every merge to `main` publishes a fresh, signed
  macOS DMG. `releases/latest` always points at the newest build
  (`macos-arm64-latest` → `Mado-latest-arm64.dmg`), and each build also
  gets an immutable, sortable `macos-arm64-r<run>-<sha>` release so you
  can always tell _exactly_ what you're running.
- **Versioned** — every `vX.Y.Z` tag cuts an official release with the
  per-platform/arch artifacts: the bare binary `mado-<os>-<arch>` +
  `.sha256`, macOS `Mado-<version>-macos-<arch>.dmg`/`.zip`, and Linux
  `Mado-<version>-linux-<arch>.tar.gz` (binary + `.desktop` + icon +
  `install.sh`). Verify with `shasum -a 256 -c mado-macos-aarch64.sha256`.

### macOS — 1. DMG (drag-install)

1. Download the DMG: the rolling
   [`Mado-latest-arm64.dmg`](https://github.com/pleme-io/mado/releases/latest)
   (Apple Silicon), or a versioned `Mado-<version>-macos-<arch>.dmg`
   (use `x86_64` for Intel).
2. Open it and drag **Mado.app** onto the **Applications** alias.
3. First launch — the app is **ad-hoc signed, not notarized**, so
   Gatekeeper blocks a plain double-click. Either **right-click →
   Open** (then confirm once), or clear the quarantine flag:

   ```bash
   xattr -dr com.apple.quarantine /Applications/Mado.app
   ```

   After that, Mado opens normally and shows up in Spotlight / Launchpad
   / the Dock.

### macOS — 2. Homebrew cask (easiest)

```bash
# until a dedicated tap repo exists, install from the raw cask file:
brew install --cask \
  https://raw.githubusercontent.com/pleme-io/mado/main/Casks/mado.rb
```

The cask tracks the rolling latest DMG and clears the quarantine flag
for you (no right-click→Open). `brew upgrade` pulls the newest merged
build. Apple Silicon only for now. See [`Casks/mado.rb`](./Casks/mado.rb)
for the tap-repo path once one is published
(`brew install --cask pleme-io/tap/mado`).

### macOS — 3. nix run (no install)

```bash
nix run github:pleme-io/mado          # run it once
nix profile install github:pleme-io/mado   # add to your profile
```

### Linux (tar.gz)

1. Download `Mado-<version>-linux-<arch>.tar.gz`.
2. Unpack and run the bundled installer (places the binary, `.desktop`
   entry, and icon under `~/.local`):

   ```bash
   tar xzf Mado-<version>-linux-<arch>.tar.gz
   ./install.sh         # → ~/.local/{bin,share/applications,share/icons}
   ```

   Ensure `~/.local/bin` is on your `PATH`. Mado then appears in the
   application menu. Or just drop the bare `mado-linux-<arch>` binary
   anywhere on `PATH`.

   Runtime deps: a GPU + Vulkan driver (mesa), Wayland **or** X11, and a
   monospace font. On Debian/Ubuntu the windowing libs are
   `libxkbcommon0 libwayland-client0`; the GPU stack is
   `mesa-vulkan-drivers`.

### Nix flake + home-manager (both platforms)

Add mado as a flake input and enable the module — you get the binary,
the desktop app bundle, and a shikumi config with fleet defaults from
one switch:

```nix
{
  inputs.mado.url = "github:pleme-io/mado";

  # In your home-manager configuration:
  imports = [ inputs.mado.homeManagerModules.default ];

  blackmatter.components.mado = {
    enable = true;       # mado binary on PATH + shikumi config
    installApp = true;   # macOS Mado.app in ~/Applications,
                         # or Linux .desktop + icon under ~/.local/share
  };
}
```

`installApp` reuses substrate's `mkDarwinAppBundle` builder on macOS
(SVG → `.icns`, typed `Info.plist`) and a typed `makeDesktopItem` on
Linux — no hand-rolled bundle logic. `nixosModules.default` and
`darwinModules.default` are also exported for system-level installs.

### Cutting a macOS release (maintainers)

The DMG/zip pipeline is a typed Rust tool, **no shell scripts**. On a
Mac with the Xcode Command Line Tools:

```bash
nix run .#release-macos -- --dry-run        # resolve the plan only
nix run .#release-macos                     # build + sign + local DMG
nix run .#release-macos -- --publish        # + push a GitHub Release
```

The tool (`release/mado-release`) builds `mado`, assembles a windowed
`Mado.app` (a real Dock icon — **not** a menu-bar app), generates the
`.icns` from `assets/mado-icon.svg`, ad-hoc codesigns, and packages the
DMG + zip. It is a thin consumer of the shared, app-agnostic
`mac-app-release` library (`release/mac-app-release`) — the extracted
core that gaveta-client's `gaveta-release` also targets. CI runs the
same tool on every merge (the autobump rolling release) and on `v*`
tags (the official cut). See [`release/mac-app-release/README.md`](./release/mac-app-release/README.md).

## Configuration

Mado reads `~/.config/mado/mado.yaml` (or `$MADO_CONFIG`) and reloads it on
save. `MADO_` environment variables override it (`MADO_FONT_SIZE=16`). The
home-manager module `blackmatter.components.mado.*` writes this file from typed
Nix options; `mado config-schema` generates those options.

```yaml
font_family: "JetBrains Mono"
font_size: 14.0
theme: vellum
window:
  padding: 8
appearance:
  opacity: 0.92            # below 1.0 asks for a composited (glass) window
links:
  editor: { kind: url_template, template: "cursor://file{path}:{line}:{col}" }
tear:
  runtime: embedded        # embedded | daemon | resident
```

Top-level groups: `font`, `window`, `shell`, `appearance`, `cursor`,
`behavior`, `theme`, `profiles`, `shaders`, `accessibility`,
`shell_integration`, `performance`, `environment`, `selection`, `search`,
`keybinds`, `quick_terminal`, `tear`, `effects`, `vigy`, `suggestions`,
`safra`, `janitors`, `links`, `feedback`, `display`, `notifications`,
`motion`, `browser`, `attachments`, `input`. Every knob is typed, and a test
holds each one live.

New customization is blue. A value in `~/.config/mado/mado.b` wins over YAML,
and each entry is refused alone when it is wrong:

```blue
use("motenashi", %i[bind config font look motion])

def mado()
  [
    look({opacity: 0.92}),
    font("JetBrains Mono", 14),
    config(:cursor, {blink: false}),
    bind("cmd+shift+y", :scroll_to_top)
  ]
end
```

Shell integration scripts for bash, zsh and fish (OSC 133, OSC 7, title) are
in `shell-integration/`; the home-manager module installs them.

## Documentation

| Doc | Subject |
|---|---|
| [`CLAUDE.md`](./CLAUDE.md) | full architecture and the working rules for this repo |
| [`docs/ROADMAP.md`](./docs/ROADMAP.md) | measured baseline and the ranked phases |
| [`docs/BLUE-WORLD.md`](./docs/BLUE-WORLD.md) | `mado.b` and blue vigies |
| [`docs/GLASS.md`](./docs/GLASS.md) | translucent window, rim, band, frost |
| [`docs/NOTIFICATIONS.md`](./docs/NOTIFICATIONS.md) | the notification system |
| [`docs/SUGGESTION-STREAM.md`](./docs/SUGGESTION-STREAM.md), [`docs/SAFRA.md`](./docs/SAFRA.md), [`docs/PREWARM.md`](./docs/PREWARM.md) | the Ctrl-S board, its sources and session prewarm |
| [`docs/MOTION.md`](./docs/MOTION.md) | the motion algebra |
| [`docs/INTEGRATION-TESTING.md`](./docs/INTEGRATION-TESTING.md) | the test layers, including the shell seam |
| [`docs/MADO-FOR-CLAUDE-CODE.md`](./docs/MADO-FOR-CLAUDE-CODE.md) | mado as the Claude Code terminal |
| [`docs/COMPETITIVE.md`](./docs/COMPETITIVE.md) | position against other terminals |
| [`docs/GAP-ANALYSIS.md`](./docs/GAP-ANALYSIS.md), [`docs/REMEDIATION-PLAN.md`](./docs/REMEDIATION-PLAN.md) | the 2026-05-31 audit and plan (historical) |

Mado is not on crates.io: another publisher owns the name. Use the DMG, the
cask or `nix run`.
