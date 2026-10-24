# mado roadmap

Measured 2026-10-24 on the live GUI (pid 62512, v0.1.211, 2 h 18 min up, one
tear session) through `mcp__mado__frame_perf`, `status`, `version`,
`config_get`, `footprint`, `top`, and the source at `origin/main` 695651e.
Every number below names its probe. A dated number rots: measure again before
you plan on it.

## Destination

mado is a terminal that you do not notice. It paints only when a pixel
changes. At idle it uses zero CPU and zero GPU. It presents a keystroke in less
than one refresh. Its motion is calm and continuous: springs that keep their
velocity when you interrupt them, Nord colours from theme tokens, flat
surfaces with crisp edges, and no shadow, blur or glow. Its memory has a
budget for each session, and it never grows past that budget. Its unsafe code
lives in a small number of named seams, and a fuzzer drives its parser every
day. Blue declarations describe its behaviour and motion: tokens, then named
springs, then interaction recipes, then surfaces. A Rust solver runs each
frame. Each of these properties has a number, and a ratchet holds the number.

## Six axes: baseline and target

Each target is a ratchet (`theory/JOEIRA.md` §6.2). The baseline is the
recorded ceiling. Growth past it is red. A shrink is banked by the ratchet's
own command. ➖ means "not measured yet". A ➖ is an infinite ceiling, so the
first phase removes the ➖ rows.

| Axis | Quantity | Baseline 2026-10-24 | Probe | Target |
|---|---|---|---|---|
| Performance | CPU frame time | ➖ no histogram; two samples 1,787 µs and 2,778 µs (`last_frame_us`) | frame_perf | p50 ≤ 2 ms, p99 ≤ 6 ms (120 Hz budget is 8.33 ms) |
| Performance | GPU frame time | ➖ no timestamp queries | none | p99 ≤ 4 ms |
| Performance | `cat` 100 MB throughput | ➖ (ghostty is on PATH; alacritty, kitty, wezterm are not) | none | ≥ ghostty on the same host |
| Safety | unsafe sites | 43 sites in 10 files; `#![deny(unsafe_code)]` in 1 file (`src/file_drop/macos.rs:1`) | `rg "unsafe \{\|unsafe fn\|unsafe impl" src` | crate-level deny, one `#[allow]` per seam, count ratchets down |
| Safety | panic sites | 878 `.unwrap()`, 406 `.expect(`, 48 `panic!(` (inline test modules included) | `rg -o` over `src` | non-test count ratchets down |
| Safety | VT parser fuzzing | ❌ no `fuzz/` dir; 9 `proptest!` blocks | `ls fuzz`, `rg proptest!` | cargo-fuzz target on `Terminal` input, nightly run, 0 crashes |
| Safety | GUI crashes | ✅ 0 Mado.app reports; 1 test-binary crash 2026-10-22 (fork in a threaded test, `tear_core::pty::PtyHandle::spawn`) | `~/Library/Logs/DiagnosticReports` | 0 per week, crash-free hours on `status` |
| Efficiency | idle redraw | ✅ `forced_scrub` 0, `epoch_scrub` 0, `animation` 13 of 80,818 paints | frame_perf `paints` | 0 paints at idle, held by a test |
| Efficiency | mispredicted paints | ⚠️ 6,804 of 80,818 frames (8.4 %) were `total_late_idle_paints`: `needs_frame` said paint, nothing changed | frame_perf | ≤ 1 % |
| Efficiency | memory | 385 MB footprint; 297 MB of it is graphics (199 MB unmapped graphics, 88 MB IOAccelerator, 9 MB IOAccelerator graphics) | `footprint 62512` | ≤ 150 MB with one session; a per-session budget |
| Efficiency | CPU while output streams | 4.4 % CPU, power 4.4 | `top -pid 62512` | ➖ idle sample not isolated (this session streamed output) |
| Smoothness | dropped frames | ➖ no counter | none | 0 dropped frames over a scroll, resize or animation fixture |
| Smoothness | byte → present | p50 2.2 ms, p90 5.4 ms, p99 11.3 ms (71,302 samples) | frame_perf | p99 ≤ 8.3 ms |
| Speed | input → present | p50 11.3 ms, p90 14.3 ms, p99 ≈ 66–70 ms (6,171 samples) | frame_perf | p50 ≤ 8.3 ms, p99 ≤ 16.7 ms |
| Speed | cold start to first frame | ➖ `phase=first_frame_rendered` logs at INFO (`src/main.rs:735`, `src/render.rs:8005`) but no surface reads it | none | ≤ 150 ms, on frame_perf |

## Verified 2026-10-24: the 2026-10-22 review, measured again

| # | Claim on 2026-10-22 | Today | Evidence |
|---|---|---|---|
| a | `forced_scrub` was 66 % of 349,777 frames (`LoadOp::Load` afterimage guard, `EPOCH_FORCE_PAINT_FRAMES = 3`) | ❌ fixed | frame_perf: `forced_scrub` 0 and `epoch_scrub` 0 of 80,818 frames. Commit 2efef0a puts the repaint behind `effects.forced_scrub.enabled`, default off (`src/render.rs:7207`, `src/render.rs:7388`). Red-run tests at `src/render.rs:12674` and `src/render.rs:12692`. The constant still exists (`src/render.rs:2186`) and the passes still use `LoadOp::Load` |
| b | the only shadow is `elevation_shadow` (`popup_elevation`, default on) plus the ishou `Shadow` token | ⚠️ changed | `effects.popup_elevation.enabled` is off by default (2efef0a). `card_shadow` returns nothing when off (`src/render.rs:268`), and `the_default_configuration_renders_no_shadow` holds it (`src/render.rs:8750`). `elevation_shadow` stays (`src/render.rs:537`). ishou `TokenSet.shadow` still exists (`ishou/crates/ishou-tokens/src/lib.rs:81`, `shadow.rs:10`); mado does not read it. New find: glow is still on by default: `feedback.exit_code_glow = true` (`src/config.rs:1530`, live config agrees) |
| c | motion is ishou-tokens Glide, Tween, Decay, Oscillator with no spring; the pill uses wall-clock `Instant` and Continuous demand | ⚠️ changed | Still no spring: `rg -i spring` in mado `src` and in `ishou-tokens/src/motion` = 0. `ayatsuri/src/logic/spring.rs:48` `step` is still not absorbed. The pill now stamps from the render clock and asks for Continuous only while it fades (2efef0a, `src/attach_pill.rs:192`). `accessibility.follow_os_reduce_motion` (default on) reads the OS setting (d48da24) |
| d | images and scrollback are unbounded-ish (1 GiB cap × 2 parsers) | ⚠️ changed | Images now have a cap: `DEFAULT_IMAGE_MAX_BYTES = 256 MiB` per terminal, refused alone (`src/terminal.rs:1043`, d48da24). Scrollback stays at `DEFAULT_SCROLLBACK_MAX_BYTES = 1 GiB` per grid (`src/terminal.rs:1036`, `src/gui_tear_attach.rs:891`) |
| e | mado does not depend on blue-lang-host | ⚠️ changed | `Cargo.lock` now holds `blue-lang-host` and 30 more `blue-lang` rows, pulled in by `vigy-eval` (`Cargo.lock:8243`), which is off by default (`vigy.enabled: false`). mado's own code names blue 0 times (`rg blue_lang src Cargo.toml`). Config and behaviour are not blue yet |

## What the 57 commits since 2026-10-10 built

- ✅ Idle is zero redraw; popup shadow off; attach pill on the frame clock (2efef0a).
- ✅ OS reduce-motion is followed; image byte cap (d48da24).
- ✅ Typed recovery overlay for a session or pane fault: `PaneFault` with six kinds, `RecoveryAction` with five (0e4b2f0, `src/recovery.rs`). `tear.recovery_menu.enabled` is on.
- ✅ tear 0.1.46–0.1.49: switch sizing, re-attach backoff, `HistoryElided`, `SnapshotTooLarge`.
- ✅ Drop and paste: screenshot promises, image drags, atomic bracketed paste, owned temp dirs with a TTL sweep.
- ✅ Notifications: ranked cards, pane alerts, one focus relay per process, `focus_pane` action.
- ✅ Links: Cmd-hover and Cmd-click for URLs, OSC 8 and `path:line:col`.
- ✅ Kitty keyboard reset at command end; session picker resync; directory session naming.

Open issues on `pleme-io/mado`: 0 (GitHub `list_issues`, state OPEN). Open
markers: `docs/GAP-ANALYSIS.md` 51, `docs/REMEDIATION-PLAN.md` 5, `src`
TODO/FIXME 6. `docs/MOTION.md` §7 M0 and M1 are done in source: the
suggestion fade uses the render clock (`src/render.rs:3797`) and the snapshot
reuses `row_scratch` (`src/render.rs:1948`).

## Phases, ranked by gain per effort

A phase starts when the phase before it has its gate. Inside a phase, items run
in parallel.

### P0 — sensors (small effort; every later gain depends on it)

You cannot ratchet a ➖. This phase turns each ➖ into a number on
`frame_perf`.

| Item | Done when | Gate |
|---|---|---|
| CPU frame-time histogram (`frame_us`, same buckets as the latency histograms) | frame_perf shows p50, p90, p99 | `perf.rs` test: every painted frame adds one sample |
| GPU frame time from wgpu timestamp queries, `None` where the adapter lacks them | frame_perf shows `gpu_frame_us` or `outcome: blind` | test: an adapter without the feature answers blind, never 0 |
| Dropped-frame counter: a present later than one refresh after its demand | frame_perf shows `dropped_frames` | fixture test drives a late present and reads 1 |
| Launch phases on frame_perf (`launch_ms.first_frame_rendered`) | the value reads from the live GUI | test: phase values are monotonic |
| Graphics and process footprint on `status` (task_info `phys_footprint`) | `status` shows `footprint_bytes` | live read matches `footprint` within 5 % |
| Throughput bench: 100 MB of mixed VT output through `Terminal`, headless; same file through `ghostty` on the same host by hand | MB/s recorded in `docs/ROADMAP.md` | criterion bench in `benches/`, CPU-only |
| One ratchet file `perf-budget.toml` read by `joeira-ratchet` (the shipped primitive) | every row in the six-axis table has a recorded ceiling | CI job `perf-ratchet`: an unrecorded row or growth is red |

### P1 — efficiency wins already in reach (small effort, measured gain)

| Item | Evidence | Done when | Gate |
|---|---|---|---|
| Cut mispredicted paints | 8.4 % of frames are `total_late_idle_paints` | ≤ 1 % over a 1 h live window | ratchet row `late_idle_ratio` |
| Hold idle = 0 redraw | `paints` show 0 scrub frames today | 0 paints over 60 s with no input and no bytes | test drives the loop 60 s of fake time; ratchet row `idle_paints = 0` |
| Glow off by default | `feedback.exit_code_glow` is on (`src/config.rs:1530`) | prescribed tier sets it off; live config shows false | test pins the default; the code stays behind the flag |
| Unrepresentable shadow | flag is off; `TokenSet.shadow` still exists | ishou `TokenSet` has no `shadow` field; `popup_elevation` draws a flat Nord border | compile: a theme that names a shadow does not build; mado test `the_default_configuration_renders_no_shadow` stays |

### P2 — speed (medium effort, the largest felt gain)

| Item | Evidence | Done when | Gate |
|---|---|---|---|
| Input → present p99 | p99 ≈ 66–70 ms against p50 11.3 ms | p99 ≤ 16.7 ms, p50 ≤ 8.3 ms | ratchet rows `input_present_p50`, `input_present_p99` |
| Find the tail first | 7,574 tear calls on the UI thread, 6,751 of them `send_keys` | a trace names the cause of every frame > 16.7 ms | a test fails when a blocking tear call runs on the UI thread |
| Cold start | ➖ until P0 | first frame ≤ 150 ms | ratchet row `launch_first_frame_ms` |

### P3 — memory (medium effort)

| Item | Evidence | Done when | Gate |
|---|---|---|---|
| Graphics footprint | 297 MB graphics of 385 MB total, one session | ≤ 150 MB total with one session | ratchet row `footprint_one_session` |
| Scrollback per session | 1 GiB byte cap per grid; mado mirror and tear both keep a grid | one byte budget per session, shared by both grids, on `status` | test: a session that passes the budget evicts and never grows |
| Image cap | 256 MiB per terminal | one cap per process, configurable | test: the sum over all terminals stays under the cap |

### P4 — smoothness and motion (medium effort; needs P0 dropped-frame counter)

| Item | Evidence | Done when | Gate |
|---|---|---|---|
| Spring in ishou-tokens | no spring in `ishou-tokens/src/motion`; `ayatsuri/src/logic/spring.rs:48` has a closed-form `step` | `ishou_tokens::motion::Spring` with ayatsuri's tests moved in, ayatsuri consumes it | proptest: settle, no overshoot past the declared damping, dt = 0 is identity |
| Interruptible motion keeps velocity | picker bar, pill, quick terminal re-target from their current velocity | a re-target mid-flight has a continuous first derivative | test samples velocity across the re-target |
| Zero dropped frames in motion | ➖ until P0 | 0 dropped frames in scroll, resize and picker fixtures at 120 Hz | ratchet row `dropped_frames_fixture = 0` |
| Crisp edges in motion | ➖ | positions snap to device pixels at rest; no sub-pixel blur after settle | pixel test on the settled frame |
| Reduced motion | ✅ `follow_os_reduce_motion` on | every surface has a reduced-motion path | test enumerates surfaces; a surface without a path is red |

### P5 — safety (medium effort, steady ratchet)

| Item | Evidence | Done when | Gate |
|---|---|---|---|
| Contain the C | 43 unsafe sites in 10 files; deny in 1 | `#![deny(unsafe_code)]` at crate root; one `#[allow]` per seam (pty, macOS drop, platform, clipboard, spawn) | ratchet row `unsafe_sites`; a stray `unsafe` does not compile |
| Fuzz the parser | no fuzz target | cargo-fuzz target over `Terminal` input and resize; 24 h with 0 crashes | nightly CI job; a corpus crash becomes a regression test |
| Panic sites | 878 unwrap, 406 expect, 48 panic (tests included) | non-test count recorded and falling | ratchet row `panic_sites_nontest` |
| Crash-free hours | 0 GUI reports today | `status` shows uptime and last crash | ratchet row `crash_reports_week = 0` |

### P6 — behaviour and motion in blue (large effort; after P4)

| Item | Evidence | Done when | Gate |
|---|---|---|---|
| mado takes `blue-lang-host` directly | today only transitive, through `vigy-eval` | one `Program` loads `.b` at start | test: a `.b` motion file changes a live spring with no rebuild |
| Motion as blue | tokens → named springs → interaction recipes → surfaces | every motion surface reads a recipe; the Rust solver stays per frame | test: each surface names a recipe row; a surface without one is red |
| Config as blue | config is YAML through shikumi | the blue entry is the source; YAML is a rendering | round-trip test: blue → YAML → blue is identity |

## Order

P0 → P1 → P2 → P3 → P4 → P5 → P6. P5 can start beside P1, because it reads
no P0 sensor. P4 waits for the P0 dropped-frame counter. P6 waits for the P4
spring.
