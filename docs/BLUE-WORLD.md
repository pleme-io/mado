# mado in blue: the world file and blue vigies

New customization of mado is written in blue. YAML keeps working.

## The file

`~/.config/mado/mado.b`, or `$MADO_WORLD`, or `$XDG_CONFIG_HOME/mado/mado.b`.

```blue
use("motenashi", %i[bind command config font look motion off])

def mado()
  [
    look({opacity: 0.92}),
    font("JetBrains Mono", 14),
    motion({bell_flash: {duration_ms: 90}}),
    config(:cursor, {blink: false}),
    bind("cmd+shift+y", :scroll_to_top),
    bind("cmd+shift+h", "hello"),
    command("hello", {})
  ]
end

def hello()
  send_text("echo hello\n")
end
```

The words come from the `motenashi` bidama, which tobira uses too. `config(group,
fields)` reaches any group of `mado.yaml`. `look` is `appearance`, and `motion` is
`motion`. `off(kind, name)` and `tune(kind, name, fields)` work on every entry.

`entry(:vigy, "clock", {every_ms: 1000})` from `kousei` sets the tick of
`vigies/clock.b`, and `off(:vigy, "clock")` takes that file out.

## The order

Each later step wins.

1. Built-in defaults.
2. `mado.yaml`, and `MADO_*` variables.
3. The active profile, and the OS reduce-motion preference.
4. `mado.b`.

A world binding replaces the built-in or YAML binding on the same chord.

## Refusal

Each field, binding and entry is refused alone, and its siblings stand. A field
the config schema refuses or does not read is a refusal by path. Refusals appear
on the Ctrl-S board as a row that starts with `mado.b:`, and in the log. A file that
fails whole leaves the YAML config in force.

## Where blue runs

Blue never runs in the window process.

| Work | Process | Steps | Clock |
|---|---|---|---|
| The world | `mado blue-world`, a child | 20,000,000 | 10 s, then killed |
| A command | `mado blue-command`, a child per press, on its own thread | 5,000,000 | 2 s, then killed |
| A vigy tick | the vigy runtime thread, outside the render loop | 5,000,000 | none |

A step budget stops a loop. It does not stop one native call, such as a huge
`range`, so the world and each command run in a child that can be killed. On Linux
the child also has a 1 GiB address space. A world has only `environment` and `clock`
from the host. A command has those, and these words: `send_text`, `run_action` and
`board`. Its effects apply on the next wake of the render loop.

A vigy tick runs in process, so a native call in a vigy can still hold its thread
and allocate. It never holds the render loop.

With a `mado.b`, the first window waits at most 400 ms for the child, because the
surface's alpha mode is fixed when the window is made. A later answer arrives as a
reload. With no file, nothing starts and nothing waits. The render loop only reads
the cached result on the reload edge: it merges the world's fields over the new
YAML there, and that code is Rust.

Saving any file under `~/.config/mado` reads the world again and syncs the vigies.

## Blue vigies

A `.b` file in `~/.config/mado/vigies` is a vigy named after the file. Its top level
runs each tick.

```blue
n = vigy_tick_count
mado_board("clock", "blue tick #{n}")
vigy_noop()
```

- `vigy.blue` is on by default, and `vigy.blue: false` turns it off alone.
- The runtime starts only when such a file exists. It then runs no heartbeat.
- `vigy.enabled: true` still starts the whole runtime, the heartbeat and the
  `mado mcp` vigy tools.
- `vigy.tick_ms` is the tick with no `every_ms`, 5 s by default.
- Removing a file removes its vigy from the store.
- A file saved while no runtime runs waits for the next start.

**Every mado intrinsic has a blue name:** `mado_version`, `mado_host`,
`mado_process_uptime_ms`, `mado_tear_attached?`, `mado_emit_event`, `mado_board` and
the `mado_browser_*` words. The lisp names stay. Blue cannot write `-` in a name, so
the lisp names alone were unreachable from blue.

**The surface is the Ctrl-S board.** `mado_board(key, title)` keeps one row per key
in the agent lane, so a vigy shows its latest result. The `vigy_*` MCP tools show it
too, and `vigy_register` now takes `language: "blue"`.
