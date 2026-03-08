# Mado (窓) — GPU-Rendered Terminal Emulator

## Build & Test

```bash
cargo build
cargo run
cargo test --lib
```

## Architecture

### Pipeline

```
Shell → PTY → vte parser → Terminal Grid → GPU Render (garasu)
                                              ↓
Input Events → winit → Terminal Grid → PTY write
```

### Modules

| Module | Purpose |
|--------|---------|
| `render.rs` | GPU rendering via garasu (wgpu context, text, shaders) |
| `terminal.rs` | VT100/xterm state machine, cell grid, cursor, scrollback |
| `pty.rs` | Unix PTY allocation (openpty), shell spawn, I/O async pipes |
| `config.rs` | shikumi ConfigDiscovery + ConfigStore with hot-reload |
| `platform.rs` | macOS (objc2) / Linux native integration |

### Shared Libraries

- **garasu**: `GpuContext`, `TextRenderer`, `ShaderPipeline` for all rendering
- **tsunagu**: `DaemonProcess` for multiplexer daemon mode
- **shikumi**: `ConfigDiscovery`, `ConfigStore<MadoConfig>` for config

### Configuration

- Config file: `~/.config/mado/mado.yaml`
- Env override: `$MADO_CONFIG`
- Env prefix: `MADO_` (e.g. `MADO_FONT_SIZE=16`)
- Hot-reload on file change (nix-darwin symlink aware)

## Design Decisions

### GPU Rendering via garasu
- All GPU code uses garasu abstractions, not raw wgpu
- Text rendering: garasu's `TextRenderer` (glyphon-backed)
- Post-processing: garasu's `ShaderPipeline` for custom WGSL effects
- Custom shaders: `~/.config/mado/shaders/*.wgsl`

### Terminal Emulation
- vte crate for escape sequence parsing
- Custom grid implementation (cells, attributes, cursor state)
- Scrollback buffer with configurable history size

### Ghostty Parity Goals
- Font rendering quality (subpixel, ligatures)
- Shader plugin API (same uniform interface as ghostty)
- Split panes and tabs
- Native macOS menu bar integration
- Kitty graphics protocol support
