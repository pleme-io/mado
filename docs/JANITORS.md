# Janitors + Fibers — reactive invariant-holders over a typed in-process bus

> **Destination first** (Operating Principle #0). mado (and, upstream, tear)
> runs a plane of vigy-style **reactive invariant-holders** — "session
> janitors" — each owning one runtime invariant, observing continuously,
> publishing every violation as a typed finding, and remediating through
> guarded typed paths. They communicate over **connective fibers**: a
> subject-addressed typed event bus that is the in-process expression of the
> fleet's NATS/vector plane. Janitors are authored as `(defvigy …)` /
> `(deffiber …)` tatara-lisp forms on the embedded vigy runtime; the fiber
> bus is a fleet substrate crate every in-process daemon composes; tear-side
> daemon janitors sweep the shared session world. **M0 (this doc's shipped
> tier) is deliberately smaller** — see the ledger below.

## What shipped in M0 (2026-07-06)

| Piece | File | Shape |
|---|---|---|
| **Fiber bus** | `src/fibers.rs` | Closed catalog-reflected `Subject` enum (`Sessions` / `Janitors` / `Board` — ALL + slug + exhaustive match, the izumi catalog idiom; **never strings**) over one `tokio::sync::broadcast` channel per subject. One typed payload family per subject (`SessionEvent`, `janitors::JanitorFinding`, `BoardEvent`); the payload IS the address (`FiberEvent::subject()` is total), so publishing to a wrong/unknown subject is unrepresentable. Process-global `fibers::bus()` (the `suggest::store()` idiom). Lagged subscribers get a typed `Lagged(n)` then resume — the publisher never blocks. |
| **Janitor plane** | `src/janitors.rs` | `Janitor` trait: `kind()` (catalog) + `subjects()` (fiber filter) + `observe(env, now) -> Vec<JanitorFinding>` + `remediate(env, finding)`. Findings ALWAYS publish (bus + `tracing`) in every mode; remediation is gated by a **shadow-first `Authority`** (`Shadow` default → `Effect` via config) — a write without an explicit Effect decision has no code path through the runner. Mockable `JanitorEnv` border (TYPED-SPEC Environment idiom). |
| **GhostSessionJanitor** | `src/janitors.rs` | Watches the embedded tear registry for **agent-owned + fully-exited + zero-subscriber** sessions holding that predicate past a grace period (janitor-side clock; a revived session resets). Remediation (Effect only) reuses the ONE guarded close path — `guarded_close_agent_session`, now shared with the kanshou `close_session` leaf (mado 030116a) — so operator sessions are structurally out of reach and an attached session is refused, never force-killed. Complements tear f9b1f39's reap-on-exit: that fix kills the class at exit time; the janitor keeps it dead at runtime (watched-at-exit-then-detached survivors, pre-fix leftovers, races). |
| **SuggestHealthJanitor** | `src/janitors.rs` | Files a board row for every lane that has **never once succeeded**. Two bars, both required: (1) `Error`/`AuthMissing`/`TimedOut` across **N consecutive completed polls** (counts polls via `last_poll_ms` movement, not janitor ticks); (2) the verdict is **`Blind`** (`crate::suggest::verdict` — no successful poll on record). A merely-`Degraded` lane (it worked before, it is failing now) files **nothing**: weather belongs in the picker's ambient health footer, and a row per blip is what teaches an operator to stop reading the rows. `Unconfigured` is deliberately absent from bar 1 — the prescribed config arms 26 sources on purpose and lets each degrade to "needs config", so an unparameterized lane is a *chosen* state, not a surprising one. Observe-only in M0 (`remediable = false`) — re-arming a source is an operator move (token/config), so the janitor surfaces it instead of pretending to fix it. |
| **Runner + cadence** | `src/janitors.rs` + `src/suggest/mod.rs` | `JanitorRunner` rides the suggest engine thread's EXISTING maintenance tick (`spawn_engine_thread`'s select loop) with a per-janitor interval gate. Chosen over vigy registration because the tick already exists, runs even with suggestions disabled (the loop parks armed), and is hot-reload-aware; the embedded vigy runtime is default-OFF and lacks the Rust-state intrinsics janitors need. The `(defvigy …)` authoring surface is the named destination; this runner is that reconciler's M0. |
| **Config** | `src/config.rs` | `janitors:` shikumi section (`deny_unknown_fields`): bare = OFF; prescribed = **shadow-on** (both janitors armed, `authority: shadow`, `board_rows: true`). Per-janitor `enabled` / `interval_secs` / `authority` override + janitor-specific knobs (`grace_secs` 180, `min_consecutive_polls` 3). **Hot-reload**: the section rides the same `EngineCommand::Swap` path as `suggestions`/`safra` — a config edit rebuilds the runner live (janitor observation state resets only when the `janitors:` section itself changed). |
| **Board integration** | `src/janitors.rs` (`RealJanitorEnv::project_to_board`) | Findings project as izumi **agent-lane rows** through the existing `suggest::inject` path (stable `janitor:<slug>:<target>` keys ⇒ idempotent upsert, one living row per finding, 🧹-prefixed session name) — pathologies surface on Ctrl-S. Gated by `janitors.board_rows`. Each projection also publishes `BoardEvent::JanitorRowInjected` on the Board subject. |

### Config example

```yaml
janitors:
  enabled: true          # prescribed default
  authority: shadow      # shadow-first; flip to `effect` to let janitors act
  board_rows: true       # findings surface on Ctrl-S (agent lane)
  ghost_session:
    enabled: true
    interval_secs: 60
    grace_secs: 180
    authority: effect    # per-janitor override (this one is safe: guarded close)
  suggest_health:
    enabled: true
    interval_secs: 120
    min_consecutive_polls: 3
```

## Prior fibers (absorbed incrementally — NOT rewired in M0)

Three point-to-point strands predate the bus and are, in hindsight,
single-subject fibers: `suggest::board_nudge` (a payload-less Notify),
izumi's `Reactive` store-generation watch, and `suggest::EngineControl`
(the config-swap mpsc). Each keeps working untouched; absorption onto typed
subjects happens one strand at a time when a second subscriber materializes
(the three-site rule). Rewiring them in M0 would churn proven seams for
zero new capability.

## Tier-honest ledger (never round up)

- **Truly unrepresentable:** an event on a nonexistent subject (the payload
  derives the address); a runner-driven remediation without an
  `Authority::Effect` decision (no code path).
- **Only-mitigated:** the attached-session refusal is a runtime guard
  re-checked at close time inside `guarded_close_agent_session` — honest
  ceiling while the registry is a shared mutable map (C4-shaped); the
  ghost predicate itself is observed state, and a subscriber attaching
  between observe and close is caught by the guard, not by a type.
- **Observe-only:** `SuggestHealthJanitor` names no remediation in M0.
- **Shadow-first posture:** prescribed default holds every fix; `effect`
  is an explicit operator flip (per janitor or global).

## Destinations (named, not started)

1. **tear-side daemon janitors** — the same trait swept over the daemon's
   session world (gated on tear's SESSION-TYPESCAPE M2 one-PracaStore arc).
2. **Fiber absorption of the legacy strands** (board_nudge, Reactive,
   EngineControl) as typed subjects, one per second-subscriber trigger.
3. **`(defvigy …)` / `(deffiber …)` tatara-lisp authoring** on the embedded
   vigy runtime once the mado intrinsics expose the tear registry + izumi
   store to lisp programs — the runner is that reconciler's M0.
4. **Fleet extraction** of the fiber bus (and the janitor runner shape) into
   an izumi-style substrate crate on the second in-process consumer.
5. **First non-test subscriber** — a notify-center bridge (Critical findings
   → native notification) or an MCP `janitor_status` leaf over
   `JanitorRunner::is_active` + recent findings.

## Tests (all in-module; suite 1219 green as of M0)

`fibers.rs`: catalog completeness/collision-freedom, payload→subject
totality, subject-scoped round-trip, no-subscriber publish, lagged-receiver
recovery. `janitors.rs`: catalog reflection, ghost grace/reset/candidate
guards, remediate-refuses-attached, health N-consecutive-polls counting
(stalled clock, recovery reset), runner shadow-vs-effect (shadow never
writes; effect closes through the guard + publishes `GhostSessionReaped`),
per-janitor authority override, interval gating, disabled-plane inertness,
board-projection gating, config tiers + `deny_unknown_fields` + authority
round-trip.
