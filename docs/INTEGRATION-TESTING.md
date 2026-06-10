# Terminal Integration-Test Harness — mado + frostmourne + frost + tear + wadachi

> Destination-led plan (Operating Principle #0). Authored 2026-06-10 from the
> freeze-on-Enter incident investigation (multi-agent diagnosis + design,
> workflow `wf_af434357-3be`). No shell anywhere: every layer is a typed Rust
> test or a clap subcommand; CI shims are 3-line YAML over substrate
> reusables; binaries resolve through Nix derivation attrs, never `$PATH`.

## Why this exists — the 2026-06-10 incident chain

One user action ("open mado, press Enter") exposed four independent defects,
none covered by any test:

| # | Defect | Where |
|---|--------|-------|
| E2 | A terminal that never answers `ESC[6n` (CPR) **fatally kills** frost — reedline's 2s timeout error was treated as fatal by the REPL | frost `crates/frost/src/main.rs` Err arm |
| E5 | **CPR answer-loss race**: post-accept-line, under frostmourne's prompt repaint traffic, the CPR answer reaches the PTY but reedline/crossterm loses it (answered-within-50ms still timed out, 5/5). Startup queries don't hit the race; post-Enter ones do | frost/reedline 0.42 (`engine.rs:699` → `painter.rs:174`) + crossterm filtered-event window |
| E3 | **follows downgrade**: `nix/flake.nix` `frostmourne.inputs.frost.follows = "frost"` makes the root frost pin the only one that deploys; bumping frostmourne's lock alone silently re-shipped fatal frost | nix repo deployment wiring |
| latent | mado's APC interceptor treated `ESC`+non-`\` as literal payload — an unterminated APC swallowed **all** later bytes incl. `ESC[6n` (not the incident's cause, found during diagnosis) | mado `src/terminal.rs` feed pre-parser |

Also found: mado MCP↔GUI kanshou forwarding broken in practice
(`list_sessions` returns 0 with a live GUI) — the introspection plane L2
depends on.

**Verification discipline:** "process alive" ≠ "shell interactive". The
gen-195 false-verification watched logs for 12s without pressing Enter.
Every layer below asserts a **command round-trip**, not liveness.

## Destination — four layers

### L0 — frost under a typed fake terminal (frost repo) — ✅ SHIPPED (M0)

`crates/frost/tests/persona_pty.rs`: a typed `TerminalPersona` owns the PTY
master with a `DsrPolicy` (`Answer{latency}` / `Mute` / `SplitReply{gap}`) and
a timed keystroke script, driving the **real frost binary**
(`CARGO_BIN_EXE_frost`). Invariants (liveness, not mechanism):

- `mute_dsr_never_fatal` — E2 codified; **fails on pre-retry frost** (proven teeth)
- `post_accept_line_survives_and_executes` — the Enter-kills-shell invariant
  (tier-honest: E5-faithful repro needs the frostmourne rc → M1 row)
- `split_escapes_roundtrip` — fragmented CPR replies must be consumed
- `answers_dsr_idle_stays_alive` — healthy-terminal baseline

L0 is the test bed for the frost-side destination: a pleme-io **reedline
fork** where `painter.rs` treats `cursor::position()` Err as
best-guess-position — CPR becomes an optimization, never a liveness
dependency. The `Mute` invariants then flip from "frozen-but-alive" to
"proceed immediately"; the personas don't change.

### L1 — mado headless engate loop (mado repo, no GPU) — M1

Seam verified to exist: `tests/embedded_engate_smoke.rs` (real tear
`InProcess` + producer, no GPU). Expansion: swap the recording consumer for
the **real `TerminalSink` + `ResponseWriter`** wired exactly as production
(`gui_tear_attach.rs`), child = **real frostmourne** passed as a Nix
derivation attr → env var. Assertions via typed probe counters on
`TerminalSink` (`queries_seen`, `responses_written`, per-query latency):

- CPR answered < 100ms at T+0 **and after Enter** (the E5 death point)
- grid shows a fresh prompt after Enter (pane snapshot)
- wall-clock soak (T+30s) as a nightly `#[ignore]` row

Plus the **class-killer parser invariant** — ✅ SHIPPED early (M0, mado
`src/terminal.rs` tests): ∀ byte-prefix of a real captured frostmourne
stream (`tests/fixtures/frostmourne-enter-cycle.bin`, marked binary — git
CRLF-normalized it on first add) **and** adversarial unterminated-APC
prefixes: `feed(prefix); feed(b"\x1b[6n")` ⇒ `take_response().is_some()`.
Shipped together with the anywhere-ESC APC fix + 8 MiB payload bound it
forces, and the `ResponseWriter` failure logging (the `let _ =` fault-hider).

### L2 — GUI E2E via MCP against the BUILT closure — M2

A `mado e2e` clap subcommand (typed rmcp client; smoke matrix in shikumi
YAML), run by a nix app in the **nix repo** that resolves `mado` and
`frostmourne` **from the built system closure** — never cargo artifacts.
This is the only layer that catches deployment wiring (the E3 follows class):
matrix row 1 asserts the **running** shell's frost rev against the flake's
declared minimum.

Smoke matrix rows: launch → prompt visible (`snapshot_grid`); Enter → fresh
prompt (the E5 row); `echo MARKER` round-trip; `cd` → **wadachi records
exactly once** (hermetic DB via env override); Cmd+G → dir-picker overlay
opens (needs a new `simulate_chord` MCP tool — `send_keys` only reaches the
PTY). Pre-req: fix kanshou GUI-forwarding (broken as of 2026-06-10).

Companion: the **closure-pin eval check** — ✅ SHIPPED (M0, nix
`parts/checks.nix`): known-fatal frost revs fail `nixos-eval`.

### L3 — fleet invariants — M3

- wadachi **single-recorder**: after `cd` via frost, drive mado
  `jump_to_recent_dir` + skim-cd; visit count stays 1
- smart-cd resolve rows; frecency parity skim-tab == wadachi-spec (proptest)
- tear `PaneGrid` passes the same VT conformance spec as mado's `Terminal`

## The typed contract — `(defterm-conformance …)` triplet

One shape consumed from both sides: a *persona* is a mock terminal (L0
drives frost with it); mado's `Terminal` / tear's `PaneGrid` are real
implementations proven against it (L1/L3). TYPED-SPEC triplet: typed Rust
border (`VtQuery`, `VtAnswer`, `AnswerPolicy`, `TerminalPersona`) + authored
Lisp spec (`specs/term-conformance.lisp`: query catalog DSR-6/DA1/DA2/
XTVERSION/OSC 10/11/DECRQSS, canonical personas, invariants) + interpreter
over a mockable `TermEnv` trait (`write`/`read(timeout)`/`now`).

Three-site rule, honestly: site 1 = frost L0 (local module today — one
consumer doesn't earn a crate). Site 2 = mado conformance → extract crate
**`espelho`** (BR-PT "mirror") at M1. Site 3 = tear `PaneGrid` at M3 proves
the shape ripened.

## Repo placement + CI wiring

| Layer | Repo | CI |
|---|---|---|
| L0 | frost `crates/frost/tests/persona_pty.rs` | cargo test / `nix flake check`; release-blocking (`needs:` in the auto-release shim) |
| L1 + proptest | mado (`tests/embedded_engate_smoke.rs` expansion; terminal.rs tests) | same shape; frostmourne as a checks-only flake input |
| L2 | mado (`mado e2e`) + nix (`apps.e2e-mado`) | self-hosted mac runner (GUI + Metal); nightly + rebuild-gating (fleet `rebuild` runs `.#e2e-mado` against the candidate closure before switch) |
| L3 | wadachi / skim-tab / nix matrix rows | per-repo `nix flake check` |
| closure-pin | nix `parts/checks.nix` | every `nixos-eval`, no runner constraints |

L0/L1 are GUI-free (GitHub-hosted runners suffice); **only L2 needs a window
server**.

## Milestones

- **M0 — SHIPPED 2026-06-10**: L0 persona harness (frost 88cd076, teeth
  proven against pre-fix frost); mado CPR-liveness ∀-prefix tests + anywhere-
  ESC APC fix + payload bound + ResponseWriter logging (mado 7748f47);
  nix frost pin bump + closure-pin check. CPR-retry deployed (frost 84b3b71).
- **M1**: L1 embedded test (real TerminalSink + real frostmourne + probe
  counters); E5-faithful frostmourne-rc persona row; extract `espelho` with
  the `(defterm-conformance …)` triplet; **start the reedline fork**
  (CPR-as-optimization — the destination that deletes the retry).
- **M2**: `mado e2e` + `simulate_chord` MCP tool + shikumi smoke matrix;
  fix kanshou GUI-forwarding; `.#e2e-mado` from the built closure on a
  self-hosted mac runner; rebuild-gate wiring.
- **M3**: L3 fleet invariants; tear adopts espelho (third site); nightly
  wall-clock soak; all gates release/rebuild-blocking.

## Incidents this harness would have caught

| Incident | Caught by |
|---|---|
| CPR death (E2, mute terminal fatal) | L0 `MuteDsr` — proven (fails on pre-fix frost) |
| Frozen prompt (E4, retry eats input) | L0 no-freeze row (arrives with the M1 fork) |
| Freeze-on-Enter / answer-loss race (E5) | L1 post-Enter CPR row + L0 frostmourne-rc row (M1) |
| follows-stale-binary (E3) | closure-pin (shipped) + L2 running-rev assertion (M2) — only a layer that runs the BUILT closure can catch deployment wiring |
| APC swallow black-hole (latent) | mado ∀-prefix CPR-liveness proptest — shipped, kills the class |
| frost picker stdio 2026-05-21 | L0 (real binary under PTY) + L2 |
