# mado, made for Claude Code

> **Destination first (Operating Principle #0).** This document names the
> *absolute-best long-term shape* of mado-as-the-Claude-Code-terminal before it
> names any deliverable. The phases at the end are the path down to that shape;
> they are not the shape. Read §1 as the north star, §7 as the route.

**Status:** design — M0 in progress. Tier-honest throughout: every contract is
tagged ✓ VERIFIED (confirmed against Claude Code on this machine / official
docs) or ◇ INFERRED (reported by research, **not** yet confirmed — never built
load-bearing until promoted to ✓).

---

## 1. The destination

**mado is the terminal a developer reaches for *because* they run Claude Code.**
Not "a terminal that tolerates Claude Code" — a terminal whose defaults,
chrome, pickers, and signals are shaped around the agentic-coding loop. Four
verbs, all default-on in the prescribed tier, all degrading silently to nothing
when Claude Code isn't present (a bare download is never worse off — see
[the standalone-experience contract](#6-default-on-posture)):

- **meld** — mado and Claude Code share state without glue. A mado pane *knows*
  which Claude Code session it hosts; Claude Code's cost/model/context surfaces
  in mado's chrome; mado's theme is the statusline's theme.
- **complement** — mado does the things a TUI agent can't do for itself:
  durable scrollback over the agent's output, search, marks, clipboard history,
  clickable `file:line`, a session picker that spans every Claude Code project.
- **assist** — mado removes friction from the loop: "Claude finished" pulls
  focus, a long task rings attention, a selection becomes the next prompt, a
  permission prompt is one keystroke.
- **go around** — mado *wraps* Claude Code: it can launch it, drive it
  headlessly, observe its transcript, and surface "what is Claude doing / what
  should I start next" without Claude Code cooperating at all.

The relationship is **bidirectional**, and the two directions are at very
different maturity:

| Direction | Channel | Today |
|---|---|---|
| **mado → Claude Code** (drive / observe / be-a-tool) | mado's MCP server | **Strong.** 48 MCP tools already (`spawn_term`, `send_keys`, `snapshot_grid`, `attention_set`, `vigy_*`, `tear_*`, …). Claude Code already lists mado + tear as stdio MCP servers in `~/.claude.json`. |
| **Claude Code → mado** (signal / correlate / surface) | hooks, statusline, transcript files, OSC | **Absent.** mado has **zero `~/.claude` awareness** and no Claude Code config/source today. This is the gap the plan fills. |

The destination is reached when both directions are first-class and on by
default: Claude Code can fully drive mado (done), **and** mado is continuously,
silently aware of every Claude Code session on the machine and shapes itself
around them.

---

## 2. Why this is mostly *extension*, not new surface

Per Operating Principle #1 (solve once; extend the near-miss primitive), almost
every capability already has a seam. The mado-surface audit (2026-06-30) found:

| Seam | File | State | Where the integration hooks in |
|---|---|---|---|
| MCP server (48 tools) | `src/mcp.rs` | complete | add a thin `claude_notify` wrapper tool; everything else reused |
| Attention (OSC 1337 RequestAttention) | `src/mcp.rs` `attention_get/set` | complete | a Claude Code `Notification`/`Stop` hook calls `attention_set` |
| Suggestion stream (27 sources) | `src/suggest/` | complete; `KurageAgents` is the only agent source | **add `ClaudeCodeSessions` source** (the M0 unlock) |
| Session naming / praça | `src/kanshou_state.rs`, praça | 80% | name a pane per Claude session; bind by project root |
| OSC 8 hyperlinks | `src/terminal.rs:3220` | parsed + interned, no click UI | make `file:line` clickable (renderer click handler) |
| OSC 133/7/0/1337 + DSR/DA writeback | `src/terminal.rs`, `src/engate_consumer.rs` | comprehensive | already parses everything an agentic CLI emits |
| Config tiers (shikumi bare/prescribed) | `src/config.rs` | complete | **add a `claude` config section** (the M0 control surface) |
| vigy reconciler runtime | `src/vigy_host.rs` | complete | a vigy can watch `~/.claude/projects` and emit state |
| `~/.claude` awareness | — | **none** | the one genuinely-new surface |

The only genuinely new things are: a `claude` config section, a
`ClaudeCodeSessions` suggestion source, a `~/.claude` reader, and (later) a
statusline binary + hook receiver. Everything else is wiring into surfaces that
already exist and are tested.

---

## 3. Grounded contracts (verified vs inferred)

mado builds **only on ✓ VERIFIED** contracts in early phases. ◇ INFERRED
contracts are promoted to ✓ (by a live-doc / live-CC check) before anything
depends on them.

### ✓ VERIFIED on this machine / docs

- **Session transcript layout** — `~/.claude/projects/<cwd-slug>/<session-id>.jsonl`,
  where `<cwd-slug>` = the cwd with every non-alphanumeric run replaced by `-`.
  This is the **correlation key**: a mado pane's cwd → slug → newest
  `*.jsonl` → that pane's live Claude Code session, with **zero CC cooperation.**
- **Statusline contract** — `settings.json → statusLine.command` receives a
  single-line JSON object on **stdin** with `model{id,display_name}`,
  `workspace{current_dir,project_dir}`, `cost{total_cost_usd,…}`,
  `context_window`/`exceeds_200k_tokens`, `output_style`, `session_id`,
  `transcript_path`, `cwd`; emits **first stdout line only**, ANSI allowed.
- **Hook I/O core** — hooks get JSON on stdin (`session_id`, `transcript_path`,
  `cwd`, `hook_event_name`, plus `tool_name`/`tool_input`/`tool_response` on
  tool events) and return a JSON decision on stdout; exit 0 = observe, exit 2 =
  block on the Pre*/Stop-class events. The user already runs a `PreToolUse(Bash)`
  → `guardrail check` command hook, so the channel is proven on this machine.
- **MCP** — stdio JSON-RPC; tool naming `mcp__<server>__<tool>`; scopes
  user(`~/.claude.json`)/project(`.mcp.json`)/managed. mado + tear already
  registered.
- **TUI** — mado already handles OSC 133 prompt marks, OSC 1337
  RequestAttention, OSC 7 cwd, OSC 8 hyperlinks, truecolor colon+semicolon SGR,
  and DSR/DA/OSC query write-back. The things that break agentic CLIs are solved.

### ◇ INFERRED — confirm before depending on

The exhaustive 29-event hook list and several per-event matcher scopes; the
`terminalSequence` hook-output key (a terminal-control escape channel — *if real,
a major direct CC→mado lever*); `CLAUDE_CODE_*` env-var names; `MAX_MCP_OUTPUT_TOKENS`;
Agent SDK package paths; the `tui: kitty` setting. Treat all as leads, not law.

---

## 4. The architecture (where each piece lives)

```
                 ┌───────────────────────── mado ─────────────────────────┐
  Claude Code    │                                                          │
  (per pane,     │   src/claude/            ← NEW: the ~/.claude reader      │
   per project)  │     mod.rs   (ClaudeHome, slug(cwd), session discovery)   │
        │        │     session.rs (typed SessionRef + transcript tail)       │
        │ writes │     spec.lisp  (TYPED-SPEC triplet: border+lisp+interp)   │
        ▼        │                                                          │
  ~/.claude/projects/<slug>/<id>.jsonl ──read──▶ suggest/sources/            │
                 │                                  claude_code_sessions.rs   │ ← M0
        │        │                               (SourceKind::ClaudeCodeSessions)│
        │ stdin  │                                                          │
        ▼        │   config.rs  → MadoClaudeConfig (bare=off / prescribed=on)│ ← M0
  statusLine ───▶│   bin/mado-statusline (or `mado statusline`) ─┐           │ ← M2
  hooks      ───▶│   `mado claude-hook` (stdin JSON → attention/ingest) ─┐   │ ← M3
                 │                                                  ▼   ▼   │
                 │   attention_set / suggest store / pane chrome ◀───────  │
                 └──────────────────────────────────────────────────────────┘
```

Two principles fix placement:

1. **The `~/.claude` reader is one typed module** (`src/claude/`), authored as a
   **TYPED-SPEC + INTERPRETER triplet** (org rule): a typed Rust border for the
   session/transcript shapes, a `(defclaudesession …)` Lisp spec, and a working
   interpreter over a mockable `ClaudeEnvironment` trait (filesystem behind the
   trait, so tests need no real `~/.claude`). The transcript schema is
   CC-internal and version-dependent — the interpreter parses **defensively**
   and returns typed `SpecError` on shapes it doesn't know, never a panic, never
   a silent wrong answer.
2. **Every emitted string is typed** (org ★★ TYPED EMISSION): the statusline
   line is a `Display` impl over a typed `StatusLine` value; hook decisions are
   `serde_json` over a typed struct; no `format!()` of ANSI/JSON.

---

## 5. The feature catalog (the full surface, by mechanism)

Grouped by the verb from §1 and tagged with the mechanism + rough effort. The
ones marked ★ are the recommended early wins (verified contract, pure-additive).

**go around — mado is aware of Claude Code (no CC cooperation needed)**
- ★ **A1 · Claude Code session source** — Ctrl-S surfaces every active/recent CC
  session (per project), Enter resumes it (`claude --resume <id>` in that cwd).
  *transcript files · M*. **← M0.**
- **A2 · pane ↔ session correlation** — a pane's cwd resolves to its live CC
  session; everything below keys off this. *transcript files · M*.
- **A3 · activity rail / "what is Claude doing"** — tail the session JSONL, show
  current tool / last action in pane chrome or a rail. *transcript tail · L*.
- **A4 · cross-project Claude dashboard** — one view of every running CC session
  fleet-wide. *transcript files · L*.

**meld — shared state**
- ★ **B1 · mado owns the statusline** — a `mado statusline` binary renders CC's
  model/cost/context in mado's *own* theme (ishou), and tees the payload into
  per-pane state for free telemetry. *statusline · M*. **← M2.**
- **B2 · context-pressure chrome** — pane tints as `context_window.used_percentage`
  climbs; `exceeds_200k_tokens` is a visible state. *statusline · S*.
- **B3 · model/effort badge** — the active model + effort shown per pane. *statusline · S*.

**assist — remove loop friction**
- ★ **C1 · "Claude finished" → attention** — a `Stop`/`Notification` hook calls
  mado's `attention_set`; the pane/dock signals done. *hook → MCP · S*. **← M3.**
- **C2 · long-task / idle ring** — idle or long-running tool → attention. *hook · S*.
- **C3 · selection → prompt** — select terminal text, one keystroke sends it as
  the next CC prompt (via `send_keys`). *keybind + MCP · M*.
- **C4 · permission fast-path** — a permission prompt becomes a mado overlay /
  one-key allow-deny. *hook + render · L · gated on ◇ permission-hook contract*.
- **C5 · clickable `file:line`** — CC prints paths constantly; make them open
  the editor. *OSC 8 / pattern-match + render click · M*.

**complement — do what the TUI can't**
- **D1 · durable block scrollback** — CC output as OSC-133 command blocks, jump
  /search/export (tear already records blocks). *OSC 133 · M*.
- **D2 · transcript-backed search** — search the *structured* transcript, not
  just the screen. *transcript · M*.

**drive — Claude Code drives mado (already strong; polish)**
- **E1 · `claude_notify` MCP tool** — one tool bundling attention + suggest-ingest
  for CC hooks to call. *MCP · S*.
- **E2 · headless launcher** — mado spawns `claude -p … --output-format stream-json`
  in a pane and renders the stream. *headless · L*.

---

## 6. Default-on posture

The user's directive is **"make everything default."** That resolves cleanly
onto mado's existing shikumi tiers — and crucially **without regressing the
standalone download** (see `MADO-DOWNLOAD-DELIGHT`-class reasoning: a bare
download must never be worse off):

- **bare tier** — Claude integration **off**. A download with no `~/.claude` and
  no Claude Code installed sees nothing new; zero filesystem probing of a
  directory that isn't there.
- **prescribed tier (fleet + the default a download falls into)** — Claude
  integration **on**, but every behavior is **presence-gated**: the
  `ClaudeCodeSessions` source yields nothing if `~/.claude/projects` is absent or
  empty (exactly like the local `RecentDirs` source yields nothing until you've
  navigated). No tokens, no network, no error spam — it simply lights up the
  moment the user actually runs Claude Code. That *is* "default-on done right":
  delightful when CC is present, invisible when it isn't.

A `claude` config section makes every piece individually overridable
(`enabled`, `home` path override via `CLAUDE_CONFIG_DIR`, per-feature toggles),
so the fleet HM module (`blackmatter-mado`) can prescribe it and a user can opt
out without touching code.

---

## 7. The route (phases — each shippable, each on verified ground)

> Phases are the path to §1, not §1 itself. Every phase is independently
> shippable and adds tests; none requires core surgery.

- **M0 — mado is aware of Claude Code, by default.** The `src/claude/` reader
  (TYPED-SPEC triplet, mockable `ClaudeEnvironment`) + `SourceKind::ClaudeCodeSessions`
  source reading `~/.claude/projects/<slug>/*.jsonl` + `MadoClaudeConfig`
  (bare-off / prescribed-on) + the source armed in `prescribed()`. Delivers A1.
  *Pure-additive, verified contract only, fully testable offline.* **← building now.**
- **M1 — pane ↔ session correlation (A2).** The cwd→slug→newest-jsonl resolver
  becomes a first-class per-pane fact other features key off. Adds an MCP
  surface (`claude_session_for_pane`) and a vigy intrinsic.
- **M2 — mado owns the statusline (B1/B2/B3).** `mado statusline` binary
  (typed `StatusLine` `Display`, ishou-themed), prescribed into
  `~/.claude/settings.json` by the HM module; tees payload into per-pane state.
- **M3 — Claude Code signals mado (C1/C2 + E1).** `mado claude-hook` stdin
  receiver + a `claude_notify` MCP tool; HM module prescribes the `Stop` /
  `Notification` hooks. **Gate:** promote the hook event/matcher contracts from
  ◇ to ✓ against live docs first.
- **M4 — loop-friction polish (C3/C5/D1).** selection→prompt, clickable
  `file:line`, block scrollback over CC output.
- **M5 — dashboards + headless (A3/A4/E2).** activity rail, cross-project view,
  headless `stream-json` launcher.

A `tela`-style **tier ledger** governs progress: every PR touching this
integration advances a phase or leaves a typed `pending-cc: <Mn>` note; nothing
builds on a ◇ contract until it is promoted to ✓.

---

## 8. Composition with fleet rules

- **TYPED-SPEC + INTERPRETER triplet** — `src/claude/` ships the Rust border +
  `(defclaudesession …)` Lisp spec + a mockable-`ClaudeEnvironment` interpreter;
  the transcript reader returns typed `SpecError`, never a panic/`todo!()`.
- **TYPED EMISSION** — statusline + hook output are `Display`/`serde` over typed
  values; no `format!()` of ANSI/JSON.
- **CATALOG REFLECTION** — `ClaudeCodeSessions` lands in `SourceKind::ALL` +
  the registry in the same commit; the existing `every_catalog_source_is_registered`
  test is the forcing function.
- **shikumi tiers** — bare-off / prescribed-on, HM-module-overridable.
- **NO SHELL** — the reader, statusline, and hook receiver are typed Rust
  subcommands of the `mado` binary, never shell scripts (the HM module wires
  them by absolute path).
- **Standalone-delight invariant** — every Claude feature is presence-gated so a
  bare download is never regressed (peer of the download-delight work).

---

## 9. Open questions / risks

- **Transcript schema drift** — CC's `*.jsonl` is explicitly internal +
  versioned. Mitigation: defensive parse + typed `SpecError`; the source
  degrades to "session exists, id + mtime known" even if line bodies don't
  parse. Never block the picker on a schema surprise.
- **The `terminalSequence` hook-output key (◇)** — if real, it's the cleanest
  direct CC→terminal control channel and would reshape M3. Confirm early.
- **Statusline ownership collision** — the user may already have a statusline
  (seki / starship). M2 must compose/optionally-delegate, not clobber.
- **Privacy** — reading `~/.claude/projects` exposes prompts/output to mado's
  process. It's the user's own data on the user's own machine, but the
  `claude.enabled` switch + presence-gating keep it opt-out and inert by default.
