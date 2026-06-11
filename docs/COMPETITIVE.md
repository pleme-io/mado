# Competitive Position (2026-06)

Synthesized 2026-06-11 from a 9-agent research pass: deep mado inventory
(file:line evidence), invariant-coverage audit, perf-architecture audit, and
competitor research (ghostty 1.3.1 / kitty 0.47.2 / alacritty 0.17 / foot
1.27 / wezterm nightly / rio 0.4.7 / iTerm2 3.7β / Warp 2.x) plus the
mid-2026 protocol-standards checklist. Companion docs: `GAP-ANALYSIS.md`
(120-capability scorecard), `REMEDIATION-PLAN.md` (M0–M7),
`MACRO-TEST-PLAN.md`, `INTEGRATION-TESTING.md`.

---

## 1. Where mado already leads

Concrete capabilities no mainstream terminal matches:

- **48-tool typed MCP automation surface** — spawn/keys/output/snapshot-grid (RLE runs)/clipboard/marks/attention/tear/vigy, all typed (`src/mcp.rs:319-1325`). Nearest peers — kitty `kitten @`, iTerm2 Python API, WezTerm Lua — are untyped and not agent-native.
- **kanshou live-GUI forwarding** — MCP reads reflect the *real rendering process* (frame perf, sessions, config), not MCP-process-local zeros (`src/kanshou_state.rs:1-40`). No competitor exposes live in-process introspection to agents.
- **Headless scenario record/replay** — `mado record` captures PTY byte streams to `*.scenario.yaml`; auto-discovered regression replay with no GPU/winit (`src/scenario.rs`, `tests/scenarios.rs`). Only alacritty's ref-tests are comparable, and those require a patched binary + Rust registration; mado regressions are drop-a-YAML.
- **Content-addressed clipboard history** — BLAKE3-128 hashed store, typed `ClipboardKind`, LRU + dedup, full MCP lifecycle (`src/clipboard_store.rs:53-235`). Unique in the field.
- **Embedded multiplexer as the default runtime** — in-process tear (tmux-class session/pane semantics, per-pane input policy, asciinema v2 recording, OSC 133 block extraction) with daemon detach-survival (`src/gui_tear_attach.rs:104-130`, `src/tear_discovery.rs`). ghostty/kitty/alacritty/rio have none; wezterm's mux is socket-based, not agent-controllable.
- **Agent-readable command blocks** — tear blocks carry exit_code + start/end timestamps over MCP (`src/mcp.rs:1208-1235`) — Warp-class blocks without the cloud/login.
- **Embedded vigy reconciler** — in-process tatara-lisp reconciler host, SQLite-persisted, 5 MCP tools, crash-isolated from the terminal (`src/vigy_host.rs:1-30`). No analogue anywhere.
- **simulate_chord injection through the real dispatch path** — awase-grammar chords resolved against the window's actual binding table on the GUI event loop (`src/action_injection.rs`, `src/mcp.rs:436`).
- **Colorblind-simulation GPU post-process** — Machado-2009 dichromacy matrices as a real render pass (`src/render.rs:596-636`). Field-unique accessibility feature.
- **Typed capability honesty** — 13-field `TerminalCaps`, DA replies are its constants, `advertised_term()` projects from implementation flags, honesty unit tests gate over-claiming (`src/caps.rs:1-212`). Only ghostty shares the honest-terminfo posture; nobody types it.
- **Typed input-resilience primitives** — `BoundedFontSize` (Refined<f32>), `KeyRepeatGate`, `ResponseWriter` + ProbeCounters (responses_written == queries_seen) close the runaway/hang bug classes by construction (`src/font_size.rs`, `src/engate_consumer.rs:40-62`).
- **frame_perf over MCP** — wait-free per-frame atomics + launch-phase timeline surfaced from the live GUI to agents (`src/perf.rs`, `src/mcp.rs:358-385`). Competitors expose nothing machine-readable.
- **Directory-frecency overlay + MCP jump** — wadachi reader picker, `jump_to_recent_dir` injecting `cd` as keystrokes (`src/dir_picker.rs`, `src/mcp.rs:565-581`).
- **∀-prefix CPR-liveness invariant testing** — every prefix of a captured real-shell stream must leave DSR answerable (`src/terminal.rs` cpr_liveness tests) — a verification posture beyond esctest/vttest spot checks.

---

## 2. Table-stakes gaps vs the field

Rows where mado is partial/✗, plus uniquely-✓ rows at bottom. Cells for
competitors derive from the research corpus (release notes, standards entry);
cells marked ⁱ are best-effort inference where the corpus was silent.

| Capability | mado | ghostty | kitty | alacritty | wezterm | iterm2 |
|---|---|---|---|---|---|---|
| Styled/colored underlines (SGR 4:1–4:5, 58/59) | ✗ (`terminal.rs:24-57` saturated u8) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Text reflow on resize | ✗ (`terminal.rs:488-515` truncate/pad) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Kitty keyboard CSI-u encoding in DEFAULT path | ✓ (shared `keybind::kitty_encode_key`, gated on the mirror's mode stack) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Mode 2027 grapheme clustering | ✗ (`terminal.rs:2603-2659` no arm) | ✓ | partial | ✗ | ✓ | ✗ |
| Mode 2048 in-band resize | ✗ (no arm) | ✓ | ✓ | ✗ | ✗ | ✓ |
| Mode 2031 theme-change notify | ✗ (no arm) | ✓ | ✗ | ✗ | ✗ | ✗ |
| OSC 8 hyperlinks clickable | partial (parsed `terminal.rs:1588`, no consumer) | ✓ | ✓ | ✓ | ✓ | ✓ |
| OSC 133 exit-status / click extensions | partial (`prompt_mark.rs:95-100` D;code discarded) | ✓ | ✓ | ✗ | ✓ | ✓ |
| Desktop notifications (OSC 9/777/99) | ✗ (drain has no consumer `terminal.rs:5789-5810`) | ✓ | ✓ | ✗ | ✓ | ✓ |
| OSC 9;4 progress bars | ✗ (mis-parsed as notification `terminal.rs:2911-2937`) | ✓ | ✓ | ✗ | ✗ⁱ | ✓ |
| XTGETTCAP reply | ✗ (`terminal.rs:2848-2861` $q/Sixel only) | ✓ | ✓ | ✗ | ✓ | ✗ |
| Complete mouse encoding (buttons+modifiers+real wheel coords) | partial (`main.rs:1368,1549-1555` button-0 hardcode, fake ;1;1) | ✓ | ✓ | ✓ | ✓ | ✓ |
| SGR-Pixels mouse (1016) + 9/1005/1015 | ✗ (no arms) | ✓ | ✓ⁱ | ✗ | ✓ | ✗ |
| Focus events (1004) emitted in DEFAULT path | ✓ (`gui_tear_attach.rs` Focused arm, mirrors `main.rs:1574-1580`) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Sixel decode/render | partial (capture-only `terminal.rs:581-595`) | ✗ | ✗ | ✗ | ✓ | ✓ |
| iTerm2 OSC 1337 File= images | ✗ (`osc_1337.rs:39-76` Unknown) | partial | ✗ | ✗ | ✓ | ✓ |
| Kitty graphics completeness (z-index honored, compression, animation, placeholders) | partial (z parsed not drawn `terminal.rs:578`) | ✓ | ✓ | ✗ | partial | ✓ |
| In-scrollback search in DEFAULT mode | ✓ (shared `search.rs` engine + `set_search` renderer hook) | ✓ | ✓ | ✓ | ✓ | ✓ |
| IME commit/preedit in DEFAULT path | partial (commit ✓; preedit rendering pending, P3) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Config hot-reload applied | ✗ (`main.rs:489-491` callback only logs) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Paste-injection safety guard | ✗ (zero PasteGuard matches) | ✓ⁱ | ✓ | partialⁱ | ✗ⁱ | ✓ⁱ |
| Live theme switch / OS dark-light follow | ✗ (boot-only `main.rs:705-710`) | ✓ | ✓ | ✗ | ✓ | ✓ |
| Audible/urgent bell | partial (visual flash only, no NSBeep) | ✓ | ✓ | ✓ⁱ | ✓ⁱ | ✓ |
| Quake/quick terminal | ✗ (dead config `config.rs:977-990`) | ✓ | ✓ | ✗ | ✗ | ✓ |
| Command palette | ✗ | ✓ | ✓ | ✗ | ✓ | ✓ |
| Vi/copy mode + hints/quick-select | ✗ | ✗ | ✓ | ✓ | ✓ | ✓ |
| Modal key tables / leader chords | ✗ (single-chord only `keybind.rs`) | ✓ | ✓ | ✗ | ✓ | ✗ |
| OSC 4 palette set/query ≥ idx 16 | partial (`terminal.rs:1645-1672` idx<16 only) | ✓ | ✓ | ✓ | ✓ | ✓ |
| Session layout save/restore | partial (daemon survives; no layout files) | ✗ | ✓ | ✗ | ✓ | ✓ |
| **Typed MCP agent automation surface** | **✓ (48 tools)** | ✗ | partial | ✗ | partial | partial |
| **Headless YAML scenario record/replay** | **✓** | ✗ | ✗ | partial | ✗ | ✗ |
| **Content-addressed clipboard history** | **✓** | ✗ | ✗ | ✗ | ✗ | ✗ |
| **Embedded reconciler runtime (vigy)** | **✓** | ✗ | ✗ | ✗ | ✗ | ✗ |
| **Colorblind GPU post-process** | **✓** | ✗ | ✗ | ✗ | ✗ | ✗ |

Already at parity (not listed): mode 2026 synchronized output, bracketed
paste, OSC 52 write (read n/a-by-design), OSC 10/11/12 + resets, DECRQSS,
DA1/DA2/DA3, DECSCUSR, truecolor SGR, box-drawing synthesis, sRGB-linear
blending, kitty-keyboard mode stack (parse/report side).

---

## 3. Prioritized implementation queue

Ranked by impact/effort within each tier. Effort: S = wiring/port of
existing code, M = new bounded subsystem, L = structural (M2-grid-class).

### P0 — bifurcation ports (exists in local-PTY path; pure port into the embedded default)

The default runtime is `TearMode::Auto` → embedded tear; anything local-only
is invisible to the actual default user. All seams: working impl in
`src/main.rs`, missing arm in `src/gui_tear_attach.rs`.

| # | Item | Impact | Effort | Seam |
|---|---|---|---|---|
| 1 | ~~**Search in embedded path**~~ SHIPPED 2026-06-11 | cmd-F dead in the default mode — top daily-use gap | S | shared `search.rs` engine + `set_search` renderer hook; Search* arms in `apply_tear_action`; overlay input routing in the tear Key arm |
| 2 | **Prompt jump + scroll/fullscreen actions in embedded path** | keyboard scrollback nav dead in default mode (only wheel works) | S | `gui_tear_attach.rs` (apply_tear_action arms only Font*/Copy/Paste/Search*); mirror `main.rs:1192-1218` (jump), `main.rs:1063-1075` (scroll/fullscreen) |
| 3 | ~~**URL click in embedded path**~~ SHIPPED 2026-06-11 | cmd-click open dead in default mode | S | tear Button arm now captures `modifiers`; `url::detect_urls` + `url_at` + `open::that` on single-click release, same gate as `main.rs:1457-1463` |
| 4 | ~~**IME commit in embedded path**~~ SHIPPED 2026-06-11 | non-ASCII input broken in default mode | S | `AppEvent::Ime(Commit)` arm in gui_tear_attach.rs → `send_keys`. Preedit rendering is separate (P3) |

### P1 — protocol table-stakes (the 2026 reviewer checklist)

| # | Item | Impact | Effort | Seam |
|---|---|---|---|---|
| 1 | ~~**Kitty CSI-u encoding in embedded path**~~ SHIPPED 2026-06-11 | default mode silently downgrades every kitty-keyboard app (nvim/helix/fish) despite advertising the mode stack | S | encoder promoted to `keybind::kitty_encode_key` (byte-wise emission, shared with main.rs); tear Key arm gates on the mirror Terminal's mode stack |
| 2 | ~~**Focus events (1004) in embedded path**~~ SHIPPED 2026-06-11 | TUIs (nvim autoread, tmux-style dim) miss focus in default mode; mode already parsed+reported `terminal.rs:2622,3051` | S | `AppEvent::Focused` arm in gui_tear_attach.rs gated on `focus_reporting()`; mirrors `main.rs:1574-1580` |
| 3 | **Mouse encoding completeness** (all buttons, modifier bits, real wheel coords, X10 coords) | right/middle-click + shift/ctrl-click broken in every TUI; wheel reports fake `;1;1` | M | encoder: `main.rs:1368,1549-1555` (SGR hardcodes button 0/no mods; X10 hardcodes 33,33), `gui_tear_attach.rs:580-707,730-738` (wheel coords) |
| 4 | **Mouse modes 9/1005/1015/1016** (1016 SGR-Pixels is the standards row) | pixel-accurate hit testing for graphics-protocol apps; legacy-app compat | S–M | add arms at `terminal.rs:2603-2659` + encoder variants; 1016 needs pixel coords already available at the mouse arm |
| 5 | **Mode 2048 in-band resize** | fixes resize-over-ssh/multiplexer races; small surface | S | DECSET arm `terminal.rs:2603-2659` + DECRQM row `terminal.rs:3024-3056`; emit `CSI 48;rows;cols;h;w t` from the resize paths (`terminal.rs:488-515` Grid::resize callers; tear reconciler `gui_tear_attach.rs:472-499`) |
| 6 | **XTGETTCAP reply** | runtime feature detection (nvim/notcurses) bypassing terminfo; projects mechanically from TerminalCaps | S–M | DCS hook `terminal.rs:2848-2861` handles only $q/Sixel — add `+q` arm answering from `caps.rs` (named M5 there) |
| 7 | **Mode 2027 grapheme clustering** | DECRQM-for-2027 is the reviewer litmus; combining handling already exists heuristically | M | arm at `terminal.rs:2603-2659`; width semantics at `terminal.rs:2776` (combining append) + `terminal.rs:2440-2530` (put_char) |
| 8 | **OSC 8 click-to-open + hover underline** | links from ls/gcc/systemd inert; "absence is disqualifying" per standards entry | M | parsed+stored `terminal.rs:1588`; needs render consumer (underline-on-hover near `render.rs:1658`) + click consumer in both mouse arms |
| 9 | **Styled + colored underlines (SGR 4:2–4:5, 58/59)** | editor diagnostics squiggles; the reason TERM is honesty-downgraded to xterm-256color | L | `CellAttrs` saturated u8 `terminal.rs:24-57` (M2 wide-Attrs restructure); SGR arms `terminal.rs:2666-2750`; underline-rect geometry `render.rs:1658,1684`; then flip `caps.rs:29` + TERM projection `pty.rs:375-389` |

### P2 — bigger structural items

| # | Item | Impact | Effort | Seam |
|---|---|---|---|---|
| 1 | **OSC 9 notifications → tsuuchi** (+ fix 9;4 mis-parse, add 777/99 subset) | long-job completion alerts — agent + human visible; everything is parse-ready | S–M | `terminal.rs:1459-1460,1607-1613` queue; `drain_notifications` test-only `terminal.rs:5789-5810` — add a production consumer in both event loops; fix 9;9/9;4 at `terminal.rs:2911-2937`; invoke tsuuchi |
| 2 | **Config hot-reload apply** | every knob edit currently requires restart; watcher already fires | M | `main.rs:489-491` callback only logs; needs ArcSwap (or equivalent) read per frame + apply-delta (REMEDIATION M4); unlocks live theme switch (`theme.rs` + `main.rs:705-710`) |
| 3 | **Paste-injection guard** | security floor: payloads with embedded `ESC[201~`/newlines go straight to PTY | S | zero PasteGuard matches in src/; wrap both paste sites `gui_tear_attach.rs:935-941` + `main.rs:1017` with a typed sanitize/confirm |
| 4 | **Sixel decode + render** | compat floor for legacy raster (wezterm/iterm2/foot ship it); capture half done | M | raw bytes already in `sixel_images` `terminal.rs:581-595,2854-2893`; decode (icy_sixel noted in-code) + draw via existing image pass (`render.rs` Pass 2.5) |
| 5 | **Reflow on resize** | every mainstream terminal rewraps; mado truncates | L | `Grid::resize` `terminal.rs:488-515`; requires LogicalLineId from the M2 grid restructure; `reflow_on_resize` knob exists `config.rs:311-330` (currently dead) |

### P3 — nice-to-have

- **OSC 133;D exit-status + duration capture** — `prompt_mark.rs:95-100` discards the code tear already carries; unlocks exit-status gutters + semantic-zone selection (S).
- **Rendered blink (SGR 5)** — stored `terminal.rs:2685,2696`, never read in render.rs (S).
- **OSC 4 palette ≥16 + OSC 104 full reset** — `terminal.rs:1645-1672` early-returns at idx 16 (S).
- **cwd inheritance into new sessions** — `terminal.rs:1561` parses OSC 7; `inherit_working_directory` dead `config.rs:567` (S).
- **Kitty graphics z-index draw order** — parsed `terminal.rs:578`, zero reads in render.rs (M); compression/animation/Unicode placeholders after.
- **Pointer-shape apply to NSCursor** — typed parse done `terminal.rs:1734-1760`; no platform consumer (S).
- **OSC 1 icon title** — `terminal.rs:2920` matches only 0|2 (S).
- **Origin-mode CUP clamp** — mode 6 tracked, CUP/VPA don't clamp `terminal.rs:3004-3012` (S).
- **G2/G3 charsets + LS2/LS3 + UTF-8 DOCS** — `terminal.rs:3474-3483` (M).
- **Frame pacer consuming target_fps/battery cap** — `main.rs:590-600` `_effective_fps` unused; `config.rs:762-812` (M).
- **Mode 2031 + live OS dark/light follow** — depends on P2 hot-reload apply (M).
- **Power-user tier** — vi/copy mode, rectangular selection, hints, command palette, leader chords (REMEDIATION M6) (L).
- **XTVERSION (CSI > 0 q)** — not in the inventory; verify and add alongside XTGETTCAP (S).
- **Dead-knob cleanup or wiring** — min_contrast, background_image/blur, quick_terminal, ShaderConfig, FontConfig extended knobs (`config.rs:440-471,872-880,977-990,900`) — wire or delete per the ConfigCoverage gate below.

---

## 4. Invariant coverage gaps

Every invariant graded partial/untested in the coverage audit (719 tests
total; pinned invariants excluded):

- [ ] **BoundedFontSize saturation** (partial) — single-pane path `main.rs:1091-1110` does raw `font_size() + 1.0`, bypassing the type entirely; `render.rs:1389` carries a DIVERGENT 6.0..=72.0 clamp (FONT_MAX is 64); no test pins that every dispatch path saturates at 64. The 2026-05-21 incident shape is still reproducible on a single-pane window.
- [ ] **KeyRepeatGate wiring** (untested) — zero mado-side tests that the gate is wired into the event loop (`gui_tear_attach.rs:434/768`); single-pane path has NO gate; removal of the gate line would pass CI.
- [ ] **NativeStylingLatch** (partial) — only the stay-armed/never-panic half is pinned (`platform.rs`); that styling actually lands on a real window needs the L2 GUI e2e row (window-server runner), which doesn't exist.
- [ ] **Selection sanitization** (untested) — no test asserts `Selection::extract_text` output is control-free under adversarial feeds; the 2026-06-11 skim-CPR fix lives in frost/skim-tab, not mado. Cheap pin: extend the `random_byte_stream` proptest to assert all-cells-printable / no bytes <0x20 except `\n`.
- [ ] **wadachi single-recorder + tear transport** (partial) — the single_recorder e2e row env-skips on CI (/bin/sh has no wadachi hook) so the assertion only runs locally; L3 rows (jump keeps visit-count at 1, frecency parity) unimplemented; the upstream tear kill_session lock-across-wait deadlock is avoided but not pinned upstream.
- [ ] **Config invariants** (partial) — MadoConfig has NO `serde(deny_unknown_fields)` (typo'd knobs silently no-op); no ConfigCoverage dead-knob guard (≥6 config groups have zero runtime consumers); coverage is ~12 spot-checks, not the 2560-cell × 3-tier cube; bounded-coherence clamps scattered at use-sites (`config.rs:1031`, `render_snow.rs:214`) instead of typed Refined bounds. (MACRO-TEST-PLAN C1.)
- [ ] **MCP surface uniformity + input-policy** (partial) — uniformity asserted for only 2 stub tools, not the ~50-tool surface (a divergent-shape tool ships silently); Locked-pane send_keys rejection has zero mado-side contract tests (enforcement trusted to tear).
- [ ] **PTY-size ⇄ rendered-grid agreement** (partial) — the `gui_tear_attach.rs:484` reconciler loop itself is untested (only its two preconditions are pinned); the end-to-end "PTY (rows,cols) == rendered (rows,cols) after any resize/font change" pin needs the L2 `.#e2e-mado` snapshot row (INTEGRATION-TESTING M2, nix wiring pending). Live open incident (task #5, Flush content inset).
- [ ] **TERM/caps honesty matrix** (partial) — `advertised_caps_have_known_status` special-cases only styled_underline; a newly-advertised cap with no probe silently passes. Fix: CAP_PROBES table + `len == as_pairs().len()` forcing gate (MACRO-TEST-PLAN M5).
- [ ] **Scenario corpus as conformance floor** (partial) — 14 hand-picked scenarios; the esctest/vttest port (60–100 cases: DECSTBM/origin/tab-stops/IRM/DECAWM/charsets) has not landed (MACRO-TEST-PLAN C2).

Pinned-but-holed (graded pinned, named holes worth closing): scrollback ring
cap never asserted (`terminal.rs:442` eviction loop — "not tested here for
brevity"); wide-char overwrite orphaning unpinned (overwrite one half →
partner cell must clear); glyph-atlas growth fully delegated to glyphon with
zero mado-side pressure tests; APC_MAX 8 MiB bound has no direct memory test.

---

## 5. Perf notes vs competitors

- **Idle full repaint is the single largest divergence.** madori runs `ControlFlow::Poll` with a self-sustaining `request_redraw` chain paced only by AutoVsync (`madori/src/app.rs:561-571`), and the seqno damage-skip was deliberately disabled for swapchain-stale-slot correctness (`render.rs:2781-2829`) — so every frame, idle or not, pays full grid snapshot clone + URL detection + rect/text rebuild + glyphon prepare at display refresh (120 Hz on ProMotion). ghostty renders on damage with 1.3's lock overhaul (most frames hold no lock); kitty renders on damage with `repaint_delay`/`input_delay` pacing; alacritty is event-driven; foot does cell-level damage. The principled fix (paint-current-slot-from-cached-state, or damage + redraw-N-after-change) is deferred to M7.
- **No backpressure or flow control.** Unbounded channels (`single_pane.rs:128-129`), no parse mailbox, no chunk coalescing, no XON/XOFF. The PTY pump holds the WRITE lock for an entire 64 KiB vte parse while the renderer clones the full grid under read locks every frame — flood ⇒ frame jitter. alacritty decouples via its synchronized double-buffer + event coalescing; ghostty via a dedicated IO thread per surface with dirty handoff. (Bounded ParseMailbox specced M2-types/M7-impl.)
- **Scrollback memory is coarse.** `VecDeque<Vec<Cell>>` at ~32 B/cell (~64 MiB worst case at 10k×200) with one Vec per row (`terminal.rs:379-396,189-211`). ghostty uses page-aligned offset-based memory (page clones via single memcpy); kitty a history ring. mado's StyleTable u16 interning groundwork (`terminal.rs:264-281`) is laid but the Cell shrink + paged/CoW grid are deferred to M2; reflow shares that restructure.
- **fps/battery caps are dead.** `_effective_fps` resolved then only debug-logged (`main.rs:590-600`); `battery_fps_cap` unimplemented (`config.rs:773-777`). kitty (input_delay/repaint_delay/sync_to_monitor), wezterm (max_fps/animation_fps), ghostty (efficiency-core scheduling on Metal) all enforce theirs.
- **Initial PTY-size handshake is heuristic.** Sessions spawn on cell_w=0.6em/cell_h=1.4em LOGICAL estimates at three sites (`main.rs:628-636`, `gui_tear_attach.rs:143-151,1042-1050,361-380`) ignoring measured advance, HiDPI, and the Flush titlebar inset; convergence is guaranteed only AFTER the first frame (grid_sync_sig latch `main.rs:1602-1621`; measured_grid reconciler `gui_tear_attach.rs:472-499`) — pre-first-frame output lays out on the wrong grid (live task #5).
- **At parity / strong:** 64 KiB PTY reads on a dedicated thread (same sizing as ghostty/kitty/alacritty/wezterm, `session.rs:332-361`); chunk-batched vte 0.15 SIMD parse with UTF-8-tail + APC handling; parking_lot RwLock (P30); DEC-2026 BSU/ESU defer with a 100 ms cap (kitty ~150 ms, `render.rs:2731-2766`) — kitty credits this technique with +20–50 % TUI throughput; refterm-style 4096-entry LRU shape cache >99 % hit + run batching + box-draw template cache (`render.rs:874-898,1959-2004,1003-1011`); mimalloc; ~3-hop keypress→PTY (~16 ms claimed embedded vs 25–45 ms daemon, `gui_tear_attach.rs:1006-1018`).
- **Instrumentation leads on agent-readability but lacks rigor.** Launch timeline + wait-free per-frame atomics + frame_perf-over-MCP (`perf.rs`, `render.rs:3104-3130`) beat everyone for agent loops, but there are no histograms/percentiles, no GPU timestamps, no input-to-photon probe — ghostty benchmarks against a 4 GB asciinema corpus; alacritty gates PRs on vtebench + typometer. Adopting vtebench/typometer as CI yardsticks would make mado's claims comparable.
