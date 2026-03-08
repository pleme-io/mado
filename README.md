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
