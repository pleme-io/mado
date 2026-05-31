# mado — Terminal Capability Gap Analysis

> Generated 2026-05-31 by a 13-agent review workflow (6 source audits of mado + 5 terminal-landscape research agents + synthesis + completeness critic). Every `have`/`partial` claim is backed by `file:line` evidence in the source audits; the load-bearing claims were independently re-verified by the critic. Companion plan: [REMEDIATION-PLAN.md](./REMEDIATION-PLAN.md).

## Executive summary

Mado is a genuinely modern GPU terminal with a best-in-class VT core (vte 0.15, complete bounds-safe cursor/erase/scroll/SGR, strong query-response discipline), real Kitty graphics + Kitty keyboard, sRGB-correct linear blending, deep performance engineering (LRU shape cache, sync-output frame defer), and a substrate-grade automation/introspection surface (~42 MCP tools, content-addressed clipboard, prompt-mark blocks, asciinema recording, embedded vigy reconciler) that NO mainstream terminal matches. Its differentiators are real and ahead of the field. But it has one structural problem that dwarfs every individual feature gap: feature bifurcation by render mode. Copy/paste, drag-selection, in-scrollback search, URL-click, mouse reporting, fullscreen, and the Kitty key encoder are ALL wired only in the legacy local-PTY fallback (main.rs); the DEFAULT runtime is TearMode::Auto, whose embedded path (gui_tear_attach.rs:20) explicitly forwards only font-zoom + raw PTY bytes. I verified this directly — the embedded input loop has no Copy/Paste/Selection/Search handling and the default is confirmed Auto. So in normal operation the terminal a user opens cannot copy, paste, select, or search. Beyond that, mado is missing a cluster of 2026 table-stakes: styled/colored underlines (4:2-4:5, 58/59 — verified entirely absent, yet TERM=xterm-ghostty advertises Smulx so editors WILL emit them and they are silently dropped), text reflow on resize (the reflow_on_resize config knob is dead — verified zero consumers), and complete mouse-button reporting (only Left forwarded). High-value shell-integration UX (exit-status, duration, semantic-zone select) is parse-only, hot-reload is half-built (watcher fires but the callback only logs), several config fields are decorative dead ends, Sixel is captured-but-never-decoded, and there is meaningful doc/code drift (the self-model lists deleted modules and a stale MCP API). Net: mado's ceiling is high and its differentiators are durable, but it currently fails the floor in its default mode — that is the headline.

## Scorecard

120 capabilities surveyed across the modern terminal landscape:

| status | count |
|---|---|
| `have` | 43 |
| `partial` | 24 |
| `missing` | 48 |
| `n/a-by-design` | 5 |

## Capability matrix

### table-stakes (36)

| capability | mado | reference terminals |
|---|---|---|
| Copy/paste (system clipboard) wired in DEFAULT mode | ❌ missing | all |
| G2/G3 + LS2/LS3 + UTF-8 DOCS charset designation | ❌ missing | xterm, Contour, foot |
| Mouse reporting wired in DEFAULT (embedded-tear) mode | ❌ missing | all |
| Search wired in DEFAULT (embedded-tear) mode | ❌ missing | all |
| Selection (word/line/multi-click) wired in DEFAULT mode | ❌ missing | all |
| Styled underlines (undercurl/dotted/dashed 4:2-4:5, double 21) | ❌ missing | kitty, ghostty, WezTerm, foot, iTerm2, VTE |
| Text reflow / rewrap on resize | ❌ missing | kitty, foot, WezTerm, ghostty, iTerm2, Contour |
| Underline color (SGR 58/59) | ❌ missing | kitty, ghostty, WezTerm, foot, iTerm2 |
| Blink attribute (SGR 5/25) actually rendered | 🟡 partial | xterm, iTerm2, kitty |
| Color emoji (COLR/CBDT) | 🟡 partial | all serious |
| Font fallback chain + configured bold/italic/bold-italic faces | 🟡 partial | kitty, ghostty, WezTerm, foot, iTerm2 |
| Fullscreen toggle (wired in default path) | 🟡 partial | all |
| Live config reload applied to running window | 🟡 partial | kitty, WezTerm, ghostty, foot, Alacritty |
| Mouse SGR (1006) + modes 1000/1002/1003 — all buttons/modifiers/coords | 🟡 partial | all (xterm SGR) |
| Origin mode (DECOM) constrains CUP to margin | 🟡 partial | xterm, kitty, foot |
| 256-color + 16-color ANSI palette | ✅ have | all |
| Bold/dim/italic(real face)/underline/strikethrough/reverse/conceal SGR | ✅ have | all |
| Bracketed paste (2004) | ✅ have | all |
| Complex text shaping (HarfBuzz/rustybuzz) | ✅ have | kitty, ghostty, WezTerm, iTerm2 |
| Configurable keybinds (rebind/unbind, YAML) | ✅ have | kitty, WezTerm, ghostty, Alacritty, foot |
| Cursor style (DECSCUSR) + visibility (DECTCEM) | ✅ have | all |
| DECSET/DECRST, DECRQM, DSR/CPR, DA1/DA2/DA3, soft+full reset | ✅ have | all |
| Focus events (1004) | ✅ have | all serious |
| Font-size live zoom + accessibility scale | ✅ have | all |
| GPU-accelerated rendering, low latency | ✅ have | kitty, ghostty, WezTerm, Alacritty, Rio |
| Grapheme clustering / wide-char / combining marks | ✅ have | kitty, ghostty, WezTerm |
| IME commit (composed text → PTY) | ✅ have | all |
| In-scrollback search (incremental, highlight, next/prev) | ✅ have | all serious |
| OSC 0/1/2 window/icon title | ✅ have | all |
| Per-profile config | ✅ have | iTerm2, Konsole, WezTerm |
| Scroll regions (DECSTBM), autowrap, tabs, charset SI/SO | ✅ have | all |
| Scrollback buffer (configurable size) | ✅ have | all |
| sRGB-correct linear blending | ✅ have | ghostty, kitty |
| Synchronized output (mode 2026) | ✅ have | kitty, ghostty, WezTerm, foot, Contour, iTerm2 |
| Truecolor 24-bit SGR (38;2/48;2) | ✅ have | kitty, ghostty, WezTerm, Alacritty, foot, iTerm2 |
| VT100/VT220 cursor/erase/scroll/index core | ✅ have | all (vte/xterm lineage) |

### high-value (63)

| capability | mado | reference terminals |
|---|---|---|
| Background blur | ❌ missing | iTerm2, ghostty, kitty, Konsole |
| Command duration capture/display | ❌ missing | iTerm2, Warp, ghostty |
| Command exit-status capture (OSC 133;D;code) | ❌ missing | iTerm2, kitty, ghostty, WezTerm |
| Command palette | ❌ missing | ghostty, WezTerm, Warp, Windows Terminal |
| cwd inheritance into new pane/tab | ❌ missing | kitty, ghostty, WezTerm, iTerm2 |
| DCS XTGETTCAP termcap query (+q) | ❌ missing | xterm, kitty, ghostty |
| Four-thread (Read+Render decoupled) model | ❌ missing | ghostty, kitty |
| High-contrast theme | ❌ missing | iTerm2, Windows Terminal |
| Hints / quick-select label-overlay mode | ❌ missing | kitty, WezTerm, Alacritty, Contour, foot |
| IME preedit / inline composition rendering | ❌ missing | foot, kitty, WezTerm, iTerm2 |
| iTerm2 inline images (OSC 1337 File=) | ❌ missing | iTerm2, WezTerm, Rio |
| Key chords / leader keys / modal key tables | ❌ missing | WezTerm, ghostty, kitty, zellij |
| Minimum-contrast enforcement (WCAG) | ❌ missing | iTerm2 |
| modifyOtherKeys / XTMODKEYS legacy disambiguation | ❌ missing | xterm, kitty |
| Mouse encodings X10(9)/urxvt(1015)/pixel-SGR(1016) | ❌ missing | xterm, kitty, WezTerm |
| Multiple OS windows / new-window action | ❌ missing | all |
| OSC 8 hyperlinks clickable + hover affordance | ❌ missing | kitty, ghostty, WezTerm, iTerm2 |
| Overline (SGR 53/55) | ❌ missing | xterm, VTE |
| Paged / CoW / style-dedup grid memory | ❌ missing | ghostty, kitty |
| Persistent scrollback / scrollback-to-file / pager | ❌ missing | kitty (pager), iTerm2 |
| Rectangular / block / columnar selection | ❌ missing | kitty, WezTerm, iTerm2, Alacritty |
| Regex / custom-pattern hyperlink matching | ❌ missing | kitty, WezTerm, Alacritty |
| Regex / vi-style scrollback search | ❌ missing | WezTerm, iTerm2, Alacritty |
| Runtime theme/font switching (live) | ❌ missing | kitty, WezTerm, ghostty, iTerm2 |
| Screen-reader / accessibility tree | ❌ missing | iTerm2, Terminal.app, VTE |
| Semantic zones (select last command output) | ❌ missing | iTerm2, kitty, ghostty, WezTerm |
| Session save/restore (persistent layout) | ❌ missing | kitty, WezTerm, zellij, tmux, iTerm2 |
| Subpixel (LCD) text AA | ❌ missing | iTerm2, ghostty |
| Symbol map (per-codepoint-range font assignment) | ❌ missing | kitty, WezTerm |
| Synthetic italic / faux-bold thicken / variable fonts | ❌ missing | kitty, ghostty, iTerm2 |
| Tab / window title templates | ❌ missing | WezTerm, iTerm2, kitty |
| vi / copy mode (keyboard scrollback nav + select) | ❌ missing | Alacritty, kitty, WezTerm, foot, ghostty, tmux, zellij |
| Automatic light/dark follow-OS | 🟡 partial | ghostty, kitty, WezTerm, iTerm2 |
| Box-drawing / block-element custom GPU glyphs | 🟡 partial | kitty, ghostty, Windows Terminal |
| Damage/dirty-region tracking (per-region, not full-frame) | 🟡 partial | foot, kitty, ghostty |
| Kitty graphics z-index honored / compression / animation / placeholders | 🟡 partial | kitty, ghostty |
| Kitty keyboard ENCODING wired in DEFAULT (embedded-tear) path | 🟡 partial | kitty, ghostty, foot |
| Mouse pointer-shape on hover (I-beam/hand) + OSC 22 → OS cursor | 🟡 partial | kitty, ghostty, WezTerm |
| Native shell-facing remote-control CLI (kitty @ / wezterm cli) | 🟡 partial | kitty, WezTerm, ghostty (AppleScript), Alacritty msg |
| OSC 1337 iTerm2 (SetMark/RequestAttention) | 🟡 partial | iTerm2, WezTerm |
| OSC 4 palette set+query (full 0..255) | 🟡 partial | xterm, kitty, ghostty |
| OSC 8 hyperlinks (parse+store) | 🟡 partial | kitty, WezTerm, foot, ghostty, iTerm2, Konsole |
| OSC 9 / 9;9 / 777 / 99 desktop notifications + progress | 🟡 partial | ghostty, kitty, foot, Windows Terminal |
| Programming ligatures + feature control (-calt/-liga) | 🟡 partial | kitty, ghostty, WezTerm, iTerm2, Contour, Rio |
| Scroll-on-output pin toggle (keep view while scrolled up) | 🟡 partial | kitty, WezTerm, iTerm2 |
| Sixel graphics decode + render | 🟡 partial | foot, Rio, Contour, WezTerm, iTerm2, Windows Terminal |
| Window decorations / transparency / opacity | 🟡 partial | all serious |
| Built-in themes / color schemes | ✅ have | ghostty, kitty, WezTerm, iTerm2 |
| Daemon multiplexer (detach/reattach, survive restart) | ✅ have | WezTerm, tmux, zellij, foot |
| DCS DECRQSS (request setting state) | ✅ have | xterm, kitty, ghostty |
| Jump-to-prompt (prev/next) | ✅ have | iTerm2, kitty, ghostty, WezTerm |
| Kitty graphics protocol (transmit/place/delete/query + GPU draw) | ✅ have | kitty, ghostty, WezTerm, Konsole |
| Kitty keyboard protocol — mode stack push/pop/query | ✅ have | kitty, ghostty, foot, WezTerm, Windows Terminal |
| OSC 10/11/12 fg/bg/cursor color set+query | ✅ have | kitty, ghostty, WezTerm |
| OSC 133 shell-integration marks (A/B/C/D parse) | ✅ have | iTerm2, kitty, ghostty, WezTerm, Windows Terminal |
| OSC 52 clipboard WRITE (set, kinds, base64) | ✅ have | xterm, kitty, WezTerm, foot, ghostty, iTerm2 |
| OSC 7 working-directory reporting (parse) | ✅ have | kitty, WezTerm, ghostty, iTerm2, Konsole, foot |
| PTY read throughput tuning (64KiB) | ✅ have | ghostty, kitty |
| Reduce-motion (disable blink/bell flash) | ✅ have | ghostty, iTerm2 |
| shikumi TieredConfig YAML (bare/discovered/prescribed) | ✅ have | (pleme-io-unique typed-tier model) |
| URL detection (linkify) + click-to-open | ✅ have | kitty, ghostty, WezTerm, iTerm2, foot |
| VT query-response writeback in embedded path (DSR/DA/OSC) | ✅ have | (real-incident-driven; most do this implicitly) |
| OSC 52 clipboard READ (query answers host) | ➖ n/a | xterm, WezTerm (gated) |

### differentiator (21)

| capability | mado | reference terminals |
|---|---|---|
| Background image | ❌ missing | iTerm2, kitty, WezTerm, Rio |
| Broadcast / sync input across panes | ❌ missing | kitty, iTerm2, tmux |
| Built-in effect catalog (CRT/scanlines/bloom/glow-on-bell) | ❌ missing | Rio, ghostty |
| Click-to-rerun / block model | ❌ missing | Warp |
| Custom GPU shader chain (user WGSL/GLSL) | ❌ missing | ghostty, Rio |
| Event hooks / output-regex triggers | ❌ missing | iTerm2 (triggers), WezTerm |
| Remote multiplexing over SSH | ❌ missing | WezTerm, kitty, tmux |
| RTL / bidi text | ❌ missing | kitty, ghostty (partial) |
| Embedded scripting (functional terminal control) | 🟡 partial | WezTerm (Lua), iTerm2 (Python), kitty (kittens) |
| Quake / dropdown / quick-terminal | 🟡 partial | ghostty, iTerm2, Windows Terminal, Yakuake |
| Colorblind post-process (Machado matrices) | ✅ have | iTerm2 (min-contrast only); mado GPU-shader-unique |
| Embedded multiplexer (mux-without-tmux) | ✅ have | WezTerm, kitty, tear (pleme-io) |
| Embedded vigy reconciler (controllers-in-terminal) | ✅ have | (mado/pleme-io-unique) |
| Headless scenario record/replay testing | ✅ have | (mado-unique; vttest is external) |
| MCP automation surface (snapshot/send/spawn/clipboard/blocks/record) | ✅ have | (mado-unique; kitty @ / wezterm cli are nearest) |
| Pane recording (asciinema) | ✅ have | (mado-unique built-in) |
| Snow / ambient composited overlay effect | ✅ have | (mado/engawa-unique) |
| Integrated AI assistant / agent | ➖ n/a | Warp, iTerm2, Windows Terminal |
| Split panes (in-window) | ➖ n/a | kitty, ghostty, WezTerm, iTerm2, Konsole, zellij |
| Tab bar UI | ➖ n/a | kitty, ghostty, WezTerm, Windows Terminal |
| Tabs (in-window) | ➖ n/a | kitty, ghostty, WezTerm, iTerm2, Konsole, Windows Terminal |

## Prioritized gaps

### P0

- **Embedded-tear (DEFAULT) mode has no copy/paste, no selection, no search, no URL-click, no mouse reporting, no Kitty-key encoding — these exist only in the legacy local-PTY fallback. Verified: gui_tear_attach.rs:20 declares the non-goals, the input loop (510) drops Copy/Paste/search, and TearMode::Auto is the confirmed default (config.rs:354, test 1574).** (XL)
  - *Why:* This is THE gap. The mode a user opens 99% of the time cannot Cmd+C/Cmd+V, drag-select, or Cmd+F. No amount of VT or graphics excellence compensates for a terminal you cannot copy from. Every individual P0/P1 below is moot until the embedded path reaches feature parity with the local-PTY path.
  - *Where:* mado: unify the two input/UX paths. Lift selection/clipboard/search/url/mouse/key-encode out of main.rs into a shared module that engate_consumer.rs / gui_tear_attach.rs and the local path both drive over the same Terminal handle. The shared keybind::madori_key_to_pty_bytes consolidation is the model to replicate for the rest.
- **Styled + colored underlines (SGR 4:2/4:3/4:4/4:5 undercurl/dotted/dashed, SGR 21 double, SGR 58/59 underline color). Verified entirely absent in terminal.rs and render.rs.** (M)
  - *Why:* 2026 table-stakes — kitty/ghostty/WezTerm/foot/iTerm2/VTE all ship the full set; it is the substrate for LSP diagnostics in Neovim/Helix. Mado advertises TERM=xterm-ghostty (pty.rs:389) whose terminfo claims Smulx/Setulc, so editors WILL emit these and mado silently drops them — actively worse than not advertising.
  - *Where:* mado terminal.rs handle_sgr (parse colon sub-params) + a wider CellAttrs (current u8 is saturated, needs a UnderlineStyle enum + underline-color field) + render.rs underline run drawing (curl/dot/dash geometry, distinct color). Underline-style geometry is a candidate engawa typed primitive.
- **Text reflow / rewrap on resize. Verified: Grid::resize truncates/pads cells; reflow_on_resize config field has zero consumers outside config.rs.** (L)
  - *Why:* Table-stakes on the primary screen across kitty/foot/WezTerm/ghostty/iTerm2/Contour. Narrowing the window mangles wrapped output — a baseline correctness expectation. The dead config knob makes it look supported when it is not.
  - *Where:* mado terminal.rs Grid — needs logical-line tracking (wrapped-flag per row) so resize can rewrap rather than per-row truncate. Couples with the planned paged-grid memory work; do the line-model refactor once to serve both.
- **Mouse reporting completeness: only the Left button is forwarded; SGR press/release hardcodes button code 0 with no button/modifier bits; scroll reports fake ;1;1 coords; motion only under SGR.** (M)
  - *Why:* Table-stakes (xterm SGR 1006 + 1000/1002/1003). Right-click menus, middle-click, drag-with-modifiers, and accurate wheel position all break — affects tmux, Neovim, lazygit, any mouse TUI. Also gated behind the P0 bifurcation (only wired in local-PTY path).
  - *Where:* mado main.rs mouse encoder — emit real button/modifier bits and true coords; forward middle/right; then move the encoder into the shared input module so it works in embedded mode.
- **Config hot-reload does not apply: the notify watcher fires but the on_reload callback only logs (main.rs:448) and the store is bound unused; no ArcSwap read in the event loop despite the doc claim.** (M)
  - *Why:* Table-stakes — kitty/WezTerm/ghostty/foot/Alacritty all reload theme/font/keybinds without restart. Mado detects changes then ignores them; theme/font apply only at boot.
  - *Where:* mado main.rs event loop — read the ArcSwap-published config each frame and re-apply theme/font/cursor/padding deltas to the live renderer. shikumi already provides load_and_watch; the gap is purely the apply side in mado.
### P1

- **Shell-integration metadata is parse-only: OSC 133;D exit-status discarded, no command duration, no semantic-zone selection of last command output.** (M)
  - *Why:* High-value — this is the headline shell-integration UX iTerm2/kitty/ghostty/WezTerm ship (exit-code prompt coloring, duration display, select-last-output). Mado parses the marks but throws away the payload; the typed PromptMark only stores {grid_row, kind}.
  - *Where:* mado prompt_mark.rs — add exit_status + timestamp fields to PromptMark; terminal.rs parse 133;D;code; selection.rs add C..D zone range API. MCP block surface already extracts blocks, so the read side is half-done.
- **Styled-underline siblings + overline (SGR 53/55) + actually rendering the blink attribute (SGR 5 stored but never drawn).** (S)
  - *Why:* Completes the SGR attribute surface power users expect; blink-stored-but-inert is a silent correctness bug. Lands naturally with the P0 underline work (same wider-attr refactor).
  - *Where:* mado terminal.rs handle_sgr + wider CellAttrs + render.rs glyph path (read BLINK, draw overline rect). Bundle with the P0 styled-underline CellAttrs widening.
- **cwd inheritance into new panes; OSC 9/9;9/777/99 notifications displayed (currently queued but tsuuchi never invoked; OSC 9;9 mis-parsed); OSC 8 hyperlinks clickable+hover; full OSC 4 palette (0..255).** (M)
  - *Why:* High-value baseline cluster. cwd-inherit is depended on by splits/tabs; notifications are how long-command/done alerts surface; clickable OSC 8 is shipped by every flagship. Several are dead-wired (drain_notifications has no consumer; inherit_working_directory unread).
  - *Where:* mado: spawn paths read term.cwd(); a notifier glue layer drains pending_notifications into tsuuchi; main.rs/render.rs add OSC 8 click+hover; terminal.rs lift OSC 4 idx cap. Notification surfacing is a candidate tsuuchi (pleme-io) typed primitive.
- **Sixel decode+render (captured but never rasterized); iTerm2 inline images (OSC 1337 File=); Kitty graphics z-index honored + compression/animation.** (L)
  - *Why:* High-value image-protocol coverage. Sixel has crossed to baseline (foot/Rio/Contour/WezTerm/iTerm2/Windows Terminal all ship it); mado stores DCS-q bytes then drops them. z-index parsed-but-ignored means layered images draw wrong.
  - *Where:* mado render.rs + a sixel decoder dep (icy_sixel per the existing comment); honor z_index in draw sort; OSC 1337 File= decode in osc_1337.rs. Image rasterization+placement is a candidate engawa render-graph node.
- **vi/copy mode, rectangular/block selection, hints/quick-select overlay, regex search, command palette, leader/chord key sequences.** (L)
  - *Why:* High-value power-user cluster shipped by kitty/WezTerm/Alacritty/foot/zellij. These are the keyboard-driven ergonomics that define the modern-terminal power tier. egaku command-palette is declared-but-unwired; awase already models chords.
  - *Where:* mado: a keyboard-mode state machine (normal/visual) over the shared input module from the P0 unification; egaku for the palette + hint overlays; awase for chord sequences. Build on the shared-input destination, not the bifurcated paths.
- **Runtime theme/font switching (MCP config_set + Rhai mado.theme() are stubs); title templates; automatic light/dark switching (detection-only); minimum-contrast enforcement + high-contrast theme (dead knobs).** (M)
  - *Why:* High-value polish. Live theme switch + OS dark/light follow are expected of the polished tier. min_contrast and follow-OS are decorative dead ends that look supported.
  - *Where:* mado theme.rs/render.rs apply path made callable at runtime (depends on the P0 hot-reload apply path); platform.rs is_dark_mode wired to theme selection; min-contrast as a render-time color adjust. Theme tokens belong in ishou/irodzuki (already the source); the switch mechanism is mado-local.
- **Kitty key encoding wired in the embedded path; XTGETTCAP (+q) reply; modifyOtherKeys/XTMODKEYS; IME preedit rendering; G2/G3+LS2/LS3+UTF-8 DOCS charsets; origin-mode CUP constraint.** (M)
  - *Why:* High-value correctness/compat cluster. Kitty encoding only in local path is part of the P0 bifurcation; XTGETTCAP-silence breaks capability probing; IME-preedit-absence means CJK composition shows nothing in-grid.
  - *Where:* mado terminal.rs (charset designators, origin-mode CUP clamp, +q reply) + main.rs/gui_tear_attach.rs (preedit overlay, embedded key-encode via the shared input module).
### P2

- **Custom GPU shader chain + built-in effect catalog (CRT/scanlines/bloom/glow-on-bell); background image + blur; cursor trail; ligature-break-under-cursor.** (L)
  - *Why:* Differentiators ghostty/Rio own. Mado already has the GPU post-process plumbing (colorblind pass) and the engawa render-graph substrate exists org-wide — this is the highest-leverage differentiator class because the substrate is already designed for it.
  - *Where:* pleme-io engawa render-graph IR (the canonical owner per org docs: v0.4 ships the CRT/scanline/bloom/glow catalog) + mado's PostProcessPipeline generalized from the hardwired colorblind shader to an engawa-driven chain. background_image/blur are dead mado config knobs to wire.
- **Functional scripting (Rhai/soushi mado.* are logging stubs); output-regex triggers; click-to-rerun/block model; native shell-facing remote-control CLI.** (L)
  - *Why:* Differentiators (WezTerm Lua, iTerm2 triggers, Warp blocks, kitty @). Mado over-claims a rich scripting API that is barely a stub. The MCP surface is the stronger automation story; the question is whether soushi scripting earns its keep vs doubling down on MCP+vigy.
  - *Where:* mado scripting.rs (give registered fns a real Terminal handle) + soushi (pleme-io); triggers as a vigy-host reconciler over output; a `mado @`-style CLI thin over the existing MCP. Prefer extending vigy/MCP (existing strength) over reviving Rhai stubs.
- **Session save/restore (persistent layout); remote multiplexing over SSH; multiple OS windows / new-window; persistent scrollback; quake mode (config stub only).** (L)
  - *Why:* Mux/window differentiators. Most belong to tear (the mux), not mado — but the consequence today is one pane per window and no layout persistence. Quake has a full typed QuickTerminalConfig with zero runtime wiring.
  - *Where:* pleme-io tear (session-layout serialization, SSH transport) is the owner for mux features; mado wires quake (config→platform window behavior) and the new-window/multi-process story. Keep mux features in tear per the existing de-overlap.
- **Four-thread (Read+Render decoupled) model + paged/CoW/style-dedup grid memory; per-region damage tracking; subpixel AA; variable fonts; bundled Nerd Font; screen-reader accessibility tree; symbol-map per-range font assignment.** (XL)
  - *Why:* High-value-but-deferred architecture + font-quality + a11y items mado already tracks as Phase 4/5. Not urgent vs the P0 floor, but the grid-memory refactor should be co-designed with the P0 reflow line-model work to avoid touching the grid twice.
  - *Where:* mado render.rs/main.rs threading; terminal.rs Grid memory (co-design with reflow); render.rs subpixel + variable-font axes; accesskit integration; garasu/cosmic-text for fonts; symbol_map as a config→font-resolver feature.

## Strengths to keep (do not regress)

- MCP automation surface (~42 typed tools): structured RLE cell-grid snapshots, content-addressed BLAKE3 clipboard history, OSC-133-derived prompt/command/output blocks, asciinema pane recording, per-pane input policy, and kanshou live-GUI forwarding so MCP reads reflect the real rendering process. No mainstream terminal (kitty @, wezterm cli) comes close to this introspection depth. Do not regress; it is mado's defining capability.
- Embedded vigy tatara-lisp reconciler runtime inside the terminal (controllers-not-runbooks at the terminal layer) with SQLite persistence and mado-state intrinsics — unique in the field.
- Embedded in-process multiplexer (tear_core::InProcess) as the DEFAULT — tmux-class session/window/pane semantics with ghostty-class single-process latency, plus a true detachable daemon for reattach across restarts.
- Typed primitives throughout: BoundedFontSize + KeyRepeatGate (makes the runaway-font bug class impossible), ResponseWriter/TerminalSink (closes the DSR/DA query-hang loop in the embedded path), PointerShape (19 CSS variants), ClipboardKind, shikumi TieredConfig (bare/discovered/prescribed tiers + MADO_TIER + config-show) — a stronger typed config and correctness story than any competitor's flat files.
- sRGB-correct linear blending (verified real, better than the doc claims) + deep perf engineering (LRU shape cache ~0-3 shape calls/frame, sync-output frame defer, blink-flip gating, box-draw template cache, ASCII run batching, 64KiB PTY reads).
- GPU-shader accessibility: Machado-2009 colorblind simulation as a real post-process pass (no competitor does dichromacy simulation), reduce-motion that genuinely suppresses bell flash + cursor blink, and the default-on engawa-snow composited overlay with host-integrated accumulation physics.
- Headless scenario record/replay (`mado record` → *.scenario.yaml → auto-discovered #[test]) + a fully headless SessionRegistry — agents can drive and assert on the terminal without a window. 648 tests. Best-in-field regression discipline.
- Pure-safe-Rust via objc2 (zero unsafe) and theme-by-construction (adding a scheme is one irodzuki preset entry, zero mado-side change).

## pleme-io substrate alignment

Destination first (per CSE): mado should converge on ONE typed input/render/UX path so that a capability proven once holds in every mode — the bifurcation that fails the default mode is the cardinal-sin local optimum (two implementations of the same UX, only one wired in the path operators use). The single highest-leverage move is to make the embedded-tear path the canonical path and lift selection/clipboard/search/url/mouse/key-encode into one shared input module that both engate_consumer and the local fallback drive over the same Terminal handle. That is mado-local code (P0), but it is the prerequisite for nearly every P1/P2 below — build the destination, then phase the features onto it.

Map of gaps to substrate primitives vs mado-local:

engawa (typed render-graph IR) OWNS the differentiator visual class: the styled-underline geometry (curl/dot/dash), the box-drawing glyph set, Sixel/image rasterization+placement nodes, and the CRT/scanline/bloom/glow-on-bell catalog (org docs already scope these as engawa v0.4). Mado's hardwired colorblind PostProcessPipeline should generalize into an engawa-driven chain — that turns "custom shaders" (a P2 differentiator) into composing typed engawa nodes rather than one-off WGSL loading. Build underline-style + image placement as engawa nodes once; mado and ayatsuri's overlay both consume them.

ishou / irodzuki OWN theming: the theme tokens are already sourced there (theme-by-construction is a shipped strength). Live theme/font switching and OS dark/light follow are the mado-local apply mechanism on top of ishou tokens — do NOT hand-author colors in mado. Minimum-contrast/high-contrast are ishou-token-derived render-time adjustments.

shikumi OWNS config: the TieredConfig surface is already best-in-field. The gaps are mado-local apply-side wiring (hot-reload callback that actually re-applies; reading the dead knobs reflow_on_resize / min_contrast / inherit_working_directory / background_image / background_blur). The discipline lesson: a config field with zero consumers is debt — every dead knob should either be wired or deleted, and a shikumi-level "every declared field has a consumer" invariant test would prevent the decorative-config class fleet-wide.

tear OWNS mux/window features: session save/restore layout serialization, SSH-transport multiplexing, and broadcast-input are tear primitives, not mado — keep them there per the existing de-overlap (mado already correctly deleted pane.rs/tab.rs).

tsuuchi OWNS notification surfacing: the OSC 9/777/99 drain → desktop notification is a typed tsuuchi consumer (currently queued-but-never-shown because tsuuchi is never invoked).

vigy/MCP is the scripting destination: prefer extending the embedded vigy reconciler + MCP surface (mado's real strength) for output-regex triggers and automation over reviving the Rhai/soushi stubs — triggers are a vigy reconciler over output, not a new scripting engine.

The wider-CellAttrs refactor (P0 styled underlines) and the logical-line grid model (P0 reflow) should be co-designed with the deferred paged-grid memory work so the grid is restructured once — touching the Cell/Grid types three times is the duplication the Prime Directive forbids.

## Appendix — completeness critic

### Dimensions the matrix initially missed

- BELL (audio + visual) is entirely absent from the matrix as a capability row, yet it is core table-stakes and mado actually IMPLEMENTS it: terminal.rs:2683-2686 sets bell_pending on 0x07, and render.rs has a bell-flash that reduce-motion suppresses (render.rs:2508). The matrix should carry: (a) BEL/visual-bell row [mado=have], (b) audible/system bell [mado status needs verify — only bell_pending+flash found, no NSBeep/audio path located], (c) urgent-bell / bell-in-unfocused-window attention. This is a genuine omission of a fully-shipped feature AND an unverified audio sub-capability.
- MULTILINE / BRACKETED-PASTE SAFETY (paste-confirm, strip-trailing-newline, paste-injection guard) is missing entirely. kitty/iTerm2/WezTerm/foot all ship a confirm-or-strip guard so a clipboard payload containing \n cannot auto-execute a command. Verified absent in mado (zero matches for paste_confirm/strip-newline/multiline guard). This is a SECURITY table-stakes row, not a nicety — and it is doubly relevant because mado's content-addressed clipboard history is a flagship feature, so paste is a first-class path.
- FLOW CONTROL (XON/XOFF, mode 1080-ish / read backpressure / DECSET 8 disable-flow) is missing from the matrix. No XON/XOFF handling found in pty.rs/terminal.rs. Modern emulators handle Ctrl-S/Ctrl-Q + PTY backpressure to avoid runaway producers; relevant to mado's 64KiB-read perf story.
- LARGE-PASTE / HUGE-OUTPUT handling and PTY backpressure as a correctness+perf dimension is absent. The matrix has 'PTY read throughput tuning (64KiB)' but not the dual: bounded write/paste chunking and not-dropping-frames-under-flood. Given the audit notes 'heavy I/O can still drop frames' under the 2-thread model, a paste/flood-resilience row belongs.
- UNICODE / CHARACTER INPUT (compose key, unicode-codepoint hex input, emoji/char picker invoke) is missing. kitty (unicode_input kitten), iTerm2, ghostty offer codepoint entry. Verified absent in mado. Differentiator/high-value omission.
- CURSOR ANIMATION / CURSOR TRAIL is only mentioned inside a P2 bundle ('cursor trail') but is not a matrix row, while it is a named ghostty/Warp/Neovim-smear differentiator. Minor, but the matrix claims exhaustiveness.
- DESKTOP-NOTIFICATION / DOCK BADGE / TASKBAR-PROGRESS as a distinct row from OSC9 text notifications. The matrix folds 'progress' into the OSC 9;9 row but omits dock-badge/taskbar-progress-bar (Windows Terminal ConEmu progress, macOS dock badge count) as the OS-integration surface. osc_1337.rs has RequestAttention (dock bounce) parse but no badge/progress — worth a dedicated row.
- SHELL-INTEGRATION INSTALL/AUTO-INJECTION (does mado auto-source its OSC-133 shell-integration script the way iTerm2/kitty/ghostty auto-inject?) is not a row. The matrix tracks 133 PARSING but not the install/inject half, which is the operator-facing reality of whether marks ever appear.
- SCROLLBACK-INDICATOR / scroll-position UI (scrollbar, scroll percentage, minimap) is omitted. Distinct from the 'scroll-on-output pin' row.
- TERMINFO / TERM string correctness + ship-our-own-terminfo as a compat dimension. The synthesis correctly notes TERM=xterm-ghostty advertises Smulx that mado drops — but the matrix has no row for 'ships/installs a correct terminfo entry matching actual capabilities,' which is the real fix (mado is advertising capabilities it lacks).

### Status corrections (matrix was over-charitable)

- 'OSC 9 / 9;9 / 777 / 99 desktop notifications + progress' marked PARTIAL is too generous — it should be MISSING (or 'parse-only, never surfaced'). Verified: handle_osc_9_notification pushes to pending_notifications (terminal.rs:1470), drain_notifications exists (1316) but has NO consumer — the audit confirms tsuuchi is never invoked and OSC 9;9 ConEmu progress is MIS-PARSED as a bogus notification. 'Partial' implies something reaches the user; nothing does. Also OSC 777/99 dialects are absent, not partial.
- 'Blink attribute (SGR 5/25) actually rendered' marked PARTIAL is wrong — should be MISSING. Verified: SGR 5 inserts CellAttrs::BLINK (terminal.rs:2535) but the render glyph path (render.rs:1900-1995) reads inverse/bold/dim/italic/hidden and NEVER reads BLINK. Stored-but-never-drawn = MISSING render, not partial. The synthesis P1 text itself says 'SGR 5 stored but never drawn' — so the matrix row contradicts the prose.
- 'Quake / dropdown / quick-terminal' marked PARTIAL overstates it. QuickTerminalConfig is a fully-typed config struct (config.rs:877-916) but has ZERO runtime wiring in platform.rs/main.rs (verified). The audit correctly calls it 'config stub only / no runtime wiring.' This is a dead config knob = MISSING, same class as background_image/min_contrast. Calling it partial makes a decorative knob look half-built.
- 'Window decorations / transparency / opacity' marked PARTIAL — the transparency sub-part is effectively MISSING: the audit states transparent:false is hardcoded and only background-clear alpha is honored. Decorations/opacity may be partial but window-level transparency is not wired. The row conflates three things at different statuses; should be split.
- 'Mouse SGR (1006) + modes 1000/1002/1003 — all buttons/modifiers/coords' marked PARTIAL undersells how broken it is. Verified: only Left button forwarded (main.rs:1238), SGR hardcodes button 0 with no modifier bits, scroll reports fake ;1;1 coords (1414), X10 fallback hardcodes 33,33 (col=row=1). 'Partial' is defensible but the row should note button/modifier/coord bits are essentially all wrong except left-press — it is closer to 'broken' than 'partial,' and it is the SAME P0 as the bifurcation (only wired in local path).
- 'IME commit (composed text → PTY)' marked HAVE is only true in the LOCAL-PTY path (main.rs:1200). In the DEFAULT embedded path, gui_tear_attach forwards only KeyEvent.text and explicitly defers special-key/IME translation (the MVP non-goals). So IME commit is ALSO bifurcated — 'have' overstates it for the default mode the user actually opens. Should be PARTIAL with the same bifurcation caveat as copy/paste/mouse.
- 'shikumi TieredConfig YAML' tier classified as HIGH-VALUE — defensible, but note the synthesis's own discipline lesson: several declared config fields (reflow_on_resize, min_contrast, inherit_working_directory, background_image, background_blur, quick_terminal) have ZERO consumers. The 'have' on TieredConfig is correct for the LOAD path but the matrix should not let a best-in-field config surface mask that ~6 fields are decorative — the config system 'has' fields it does not honor.
- 'Focus events (1004)' HAVE is correct and verified (terminal.rs:2472/2502/2911) — but like mouse/copy, confirm it is actually emitted in the DEFAULT embedded path, not just parsed in Terminal. The Terminal-level parse is real; the question (unverified) is whether the embedded loop ever sends focus-in/out, since gui_tear_attach only forwards text + zoom. Likely PARTIAL in default mode by the same bifurcation logic.
- 'Origin mode (DECOM) constrains CUP to margin' marked PARTIAL is correct and verified (terminal.rs:3004-3012 sets the flag but CUP does not clamp to scroll region) — good call, keep as partial/effectively-missing.

### Priority disputes

- Multiline/bracketed-paste SAFETY is a missing dimension that, once added, belongs at P0/P1, NOT lower — it is a security floor (paste-injection of newline-bearing commands) shipped by every flagship, and it directly touches mado's flagship clipboard-history path. It is currently nowhere in the prioritization. Higher priority than several P2 differentiators.
- BELL is mis-prioritized by omission: visual bell is SHIPPED (so it would be a 'keep' strength row), but the audible/urgent-bell + bell-in-unfocused-window attention path is unverified/likely-missing and is table-stakes — that gap belongs at P1, co-located with the OSC 9 notification-surfacing P1 (both are 'attention/alert never reaches the user' bugs sharing the same missing notifier glue layer).
- TERMINFO correctness should be promoted into the P0 styled-underline gap as its load-bearing twin. The synthesis correctly identifies that TERM=xterm-ghostty advertises Smulx/Setulc that mado drops — but the prioritized fix is only 'add styled underlines.' The equally-valid (and far cheaper) interim fix is 'advertise a TERM that matches actual capabilities' so editors stop emitting sequences mado silently drops. Right now mado is actively lying about its capabilities; that is a correctness-floor item, and shipping a correct terminfo is S-effort vs M for full undercurl. The prioritization omits the cheap correctness half.
- Flow control / large-paste backpressure is unprioritized but couples tightly with the P2 four-thread + paged-grid work AND the noted 'heavy I/O can drop frames' — it should be called out as part of the P2 threading bundle's correctness rationale, not left implicit.
- The P2 'Functional scripting (Rhai/soushi stubs)' item is arguably MIS-prioritized as worth-doing at all. The synthesis itself recommends 'prefer extending vigy/MCP over reviving Rhai stubs' — so the genuine action is DELETE the over-claimed Rhai surface (it is documented as feature-complete but is 4 logging stubs), which is a doc/code-drift correctness fix, not a P2 feature. Reviving Rhai duplicates the MCP+vigy strength and violates solve-once. The priority should be 'remove the stub + fix the doc drift,' not 'give the stubs a real Terminal handle.'
- Config-dead-knob cleanup is under-weighted. The synthesis names ~6 dead knobs (reflow_on_resize, min_contrast, inherit_working_directory, background_image, background_blur, quick_terminal) but scatters them across P0/P1/P2. A single P1 'wire-or-delete every zero-consumer config field + add the shikumi every-declared-field-has-a-consumer invariant test' would close the entire decorative-config class at once and prevent recurrence fleet-wide — higher leverage than fixing knobs one at a time, and directly serves the Prime Directive (the invariant test is the compounding move).

### Critic verdict

The gap analysis is unusually rigorous and its load-bearing claims are VERIFIED-TRUE against the source: the render-mode bifurcation (default = TearMode::Auto → gui_tear_attach::try_run_default, whose input loop at gui_tear_attach.rs:500-515 forwards only font-zoom and explicitly defers Copy/Paste/Search/mouse/IME), the total absence of styled/colored underlines + overline (CellAttrs is a fully-saturated u8 with all 8 bits consumed — zero room for an underline-style enum without widening, exactly as claimed), the dead reflow_on_resize knob (Grid::resize truncates/pads, no logical-line rewrap), the no-op hot-reload (callback only tracing::debug!, _config_store unused, no ArcSwap read), and the broken mouse encoder (left-only, fake ;1;1 scroll coords, hardcoded X10 33,33). Prioritization is correct on the headline: the bifurcation IS the cardinal-sin local optimum and the prerequisite for nearly everything else. The status column has a handful of over-generous PARTIALs that should be MISSING (blink-render, OSC9-notifications, quake, transparency) — each contradicted by the analysis's own prose, indicating the matrix was filled slightly more charitably than the audits warrant. The matrix is also missing ~10 real dimensions, the most consequential being multiline/bracketed-paste SAFETY (a security floor every flagship ships, doubly relevant to mado's flagship clipboard) and BELL (which is partly shipped, partly missing, and entirely absent as a row). SINGLE MOST IMPORTANT MISSING CORE PATTERN: there is no row/priority for shipping a CORRECT TERMINFO/TERM string that matches mado's actual capabilities. Mado advertises xterm-ghostty (Smulx/Setulc/etc.) and then silently drops what editors emit — it is actively lying about its capability set. That is the cheapest, highest-correctness-per-effort fix (S-effort interim for the M-effort underline work) and the analysis omits it entirely. If only ONE thing is added: the capability-honesty pattern — make the advertised TERM a typed projection of the actually-implemented capability set, so 'what we claim' can never drift from 'what we render' (the same solve-once/invariant discipline the Prime Directive demands, and the natural twin of the synthesis's own dead-config-knob lesson).
