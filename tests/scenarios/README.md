# mado scenarios

Each `*.scenario.yaml` is a permanent regression test driven by
`tests/scenarios.rs` via `mado scenario-run <path>`. The runner spawns
a fresh in-process [`Session`](../../src/session.rs) — no MCP, no
winit, no GPU — replays the steps, snapshots the grid, and asserts.

## Adding a scenario

1. Capture the byte stream from a real shell:

   ```bash
   mado record --output my-bug.scenario.yaml -- zsh -c '<reproducer>'
   ```

2. Open the generated YAML, write `expect:` assertions for the cells
   you care about. Use `mado scenario-run my-bug.scenario.yaml` to
   iterate locally.

3. Drop the file into this directory and run
   `cargo test --test scenarios`. The discovery is automatic — no
   source edits needed.

## File format

```yaml
name: <kebab-case-name>            # required, must be unique
description: |                     # multi-line OK
  what this scenario proves
cols: 80                           # default 80
rows: 24                           # default 24
shell: /bin/sh                     # default /bin/sh
cwd: ""                            # ~ expands; "" = inherit
env: {}
steps:
  - kind: send
    text: "echo hi\n"              # \n \r \t \xHH \e \0 honoured
  - kind: wait_for_text
    text: "hi"
    timeout_ms: 500
  - kind: wait_ms
    ms: 50
  - kind: resize
    cols: 100
    rows: 30
  - kind: reset                    # \x1bc
expect:
  text_contains: ["hi"]            # every needle must appear
  text_not_contains: []
  cursor:
    row: 0
    col: 0
    visible: true
  cells:
    - row: 1                       # 0-indexed
      col: 0
      ch: "h"
      fg: [255, 255, 255]
      bg: [0, 0, 0]
      attrs: 0                     # bit-OR of CellAttrs bits
      width: 1
  cols: 100                        # dimensions after replay
  rows: 30
```

## What to put here

- **Real bug reproducers.** Every operator-visible bug should land here
  before its fix lands.
- **Real app sessions.** atuin, fzf, vim, htop, ranger — every CLI
  someone runs in mado deserves a scenario.
- **VT/xterm sequences with known semantics.** SGR matrix, alt-screen
  enter/exit, scroll regions, charset switches.

## What NOT to put here

- Speculative scenarios that don't trace to a real bug or a real
  promise. The corpus is a substrate of provable behaviour, not a
  list of things we wish were true.
- Scenarios that depend on host state (specific shell version,
  installed binaries). Use `/bin/sh` and POSIX builtins where possible.

## Cse-lint contract

The `scenario-corpus-present` invariant requires every garasu-app
crate to ship at least one scenario. The `mcp-stdout-clean` invariant
requires the `mcp` subcommand to route tracing to stderr. Together
they form the "every GPU app is provably debuggable" gate.
