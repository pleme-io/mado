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

Mado installs two ways: **prebuilt GitHub Release artifacts** (no Nix
needed) or the **flake + home-manager module** (Nix consumers). Both
channels build the macOS `.app` / Linux `.desktop` from the same icon
(`assets/mado-icon.svg`).

Releases live at <https://github.com/pleme-io/mado/releases>. Every tag
attaches, per platform/arch:

- the bare binary `mado-<os>-<arch>` + a `.sha256` sidecar,
- macOS: `Mado-<version>-macos-<arch>.dmg` (drag-install) and a
  `Mado-<version>-macos-<arch>.zip` of the bare `Mado.app`,
- Linux: `Mado-<version>-linux-<arch>.tar.gz` (binary + `.desktop` +
  icon + `install.sh`).

Verify any download with its checksum, e.g.
`shasum -a 256 -c mado-macos-aarch64.sha256`.

### macOS (DMG)

1. Download `Mado-<version>-macos-<arch>.dmg` (use `aarch64` for Apple
   Silicon, `x86_64` for Intel).
2. Open it and drag **Mado.app** onto the **Applications** alias.
3. First launch — the app is **ad-hoc signed, not notarized**, so
   Gatekeeper blocks a plain double-click. Either **right-click →
   Open** (then confirm once), or clear the quarantine flag:

   ```bash
   xattr -dr com.apple.quarantine /Applications/Mado.app
   ```

   After that, Mado opens normally and shows up in Spotlight / Launchpad
   / the Dock.

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
