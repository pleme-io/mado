# Mado (窓)

GPU-rendered terminal emulator following Ghostty's philosophy, written in Rust.

## Philosophy

- Native GPU rendering via wgpu (Metal on macOS, Vulkan on Linux)
- Fast, correct VT100/xterm emulation
- Platform-native integration
- WGSL shader plugins for visual effects
- Hot-reloadable configuration via shikumi
- Zero-latency input handling

## Architecture

| Module | Purpose |
|--------|---------|
| `render` | GPU pipeline via garasu (wgpu + glyphon text) |
| `terminal` | VT100/xterm state machine (vte crate) |
| `pty` | Unix PTY allocation and shell process management |
| `config` | shikumi-based config with hot-reload |
| `platform` | macOS/Linux native integration |

## Dependencies

- **garasu** — GPU rendering engine (wgpu + winit + glyphon)
- **tsunagu** — daemon IPC (multiplexer mode)
- **shikumi** — config discovery + hot-reload

## Build

```bash
cargo build
cargo run
cargo test --lib
```

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
  can always tell *exactly* what you're running.
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

`~/.config/mado/mado.yaml`

```yaml
font_family: "JetBrains Mono"
font_size: 14.0
window:
  width: 1200
  height: 800
  padding: 8
appearance:
  background: "#2e3440"
  foreground: "#eceff4"
  opacity: 1.0
```
