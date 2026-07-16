# Mado Motion — the data-first animation algebra (tier-honest)

> **What this is.** mado's animation surfaces re-expressed as **data**: every
> keyframe / tween / decay / oscillator value is a pure function of
> `(typed declaration, elapsed/dt)` on the injected render clock — no
> imperative per-frame `tick()` mutation for that class. Authored 2026-07-16
> from a `/big-bang-pleme` + `/algorithmic-prowess-seal` pass (recon →
> judge-panel design → adversarial verify). Companion to
> [`MACRO-VOCABULARY.md`](./MACRO-VOCABULARY.md) (whose "maximize macros ≠
> derive everything" core learning governs how far this goes) and
> [`REMEDIATION-PLAN.md`](./REMEDIATION-PLAN.md).

> **★ The one honesty that frames everything below (do not round up).** This is
> a **MODEST, SCOPED Layer-B win** over ~4 tween sites + 1 oscillator — it is
> *not* a universal motion algebra, and it is *not* mado's highest-value
> performance move. Two cheaper wins **dwarf its payload** (see §5). The
> algebra earns its keep as a drift-class collapse for a *narrow* family; every
> claim here is graded, and the particle sims that can't be data stay bespoke
> Rust **by construction**.

---

## 1. The destination (named first)

**Motion is data, timing is dt, values are `Refined`, and the particle sims that
can't be data stay bespoke Rust behind the same clock — nothing rounds up.**

One evaluator — `advance(motion, dt) → f32` — folds a typed motion declaration
over the render clock. Four CPU arms, each a distinct temporal *family* (a pure
lerp does **not** subsume a square wave or an open-ended decay — they are kept
type-distinct, never force-fit under one interpolant):

| Arm | Type | Shape | Real instance |
|---|---|---|---|
| **Tween** | `motion::Tween` | `from → to` over a duration, eased by a `Curve` | bell-flash (shipped), fade-in/out, slide/scale |
| **Decay** | `motion::Decay` + `motion::frame_decay` | exponential falloff, no endpoint | glow-on-bell, snow typing-pulse |
| **Oscillator** | `motion::Oscillator` + `motion::blink_on` | periodic square/sine | cursor blink, SGR-5 blink |
| **Integrator** | `ScrollKinetics` (pre-existing) | velocity·friction physical glide | scroll momentum |

The unifying contract is one trait — `motion::Advance { advance(dt) → f32;
value(); is_active() }` — with a **strict `dt == 0` no-op** (the same
determinism contract `ScrollKinetics::tick` and the L1/L2 render ladders hold).

Curves reuse the fleet's cubic-bézier vocabulary and are evaluated with the
canonical **`UnitBezier` Newton–Raphson solve browsers use** (the best-fit
classical algorithm — deterministic, allocation-free, correct to `1e-6`, with a
bisection fallback).

**Fleet-extraction destination:** the evaluator (`Curve::ease` + the arms) lifts
into **`ishou_tokens::motion`** — which already ships the curve/duration *tokens*
but has **zero evaluator** — exactly as `Refined<T,B>` was lifted out of mado's
`font_size.rs`. mado is the first consumer + proving ground; extraction lands at
the **3rd fleet consumer** (Pillar 12 / the 3-use rule), not before. No new crate
name is minted — the fleet home already exists.

---

## 2. Reuse map (verified in source — Care #4)

**EXTEND (real near-miss, fix-then-use):**

- **`ishou-tokens/src/motion.rs`** — the strongest near-miss: ships
  `Cubic(f32,f32,f32,f32)` tuples + `Durations` + `Easings` as **DATA only**
  (one `impl Default for Motion`, **zero evaluator** — verified, 58 lines). mado
  reuses the curve tuples **verbatim** (never re-mints durations/easings) and
  supplies the missing `Cubic::eval` + `Tween<T>` sampler. *Honest grade:* the
  evaluator half is **net-new code authored into an existing crate** (add a `fn`
  to a struct that has none), **not** "the primitive is 80% there."
- **`ishou_tokens::Refined<T,Bounds>`** — shipped, proven in mado's
  `BoundedFontSize`; reused for `Seconds` / `Unit` (progress + duration bounds).
  *Tier:* **only-mitigated** (see §6) — a runtime clamp, not a compile-time
  refusal.
- **mado `ctx.{elapsed,dt}` render clock + the L1/L2 dt=0 byte-determinism
  contract** — shipped; the injectable clock exists for free. Every ported arm
  reads it.
- **`engawa-wgpu` catalog param sinks** (`snow.rs` `set_time`/`set_typing_pulse`,
  `glow_on_bell.rs` `decay`) — the interpreter *writes into* these; **zero
  engawa/engawa-wgpu render-path change** to adopt (the biggest de-risk).
- **`rect_constructors!` / `dec_private_modes!`** — the shipped local Layer-B
  table-macro mechanism to imitate for the M2 `easing_curves!` table.
- **`vt.rs` byte-pin idiom** + **`garasu::headless::frame_hash` + `scenario.rs`
  L3 golden + `MADO_GOLDEN_UPDATE=1`** + the `two_identical_renders…` /
  `thirty_two_consecutive_renders…` determinism tests + **`proptest`** — the
  verification harness. No new golden harness needed.

**Correction folded in (adversarial verify):** engawa's **core IR is purely
spatial — zero temporal concept** (verified: `time|ease|tween|keyframe|elapsed`
= 0 non-doc hits in `engawa/src`). The temporal layer is a **NEW module
consuming engawa-wgpu's sinks**, not an extension of an engawa concept.
`engawa-wgpu/src/catalog/glow_on_bell.rs` *itself documents* the
`0.92^(dt·60)` decay as shared with snow's typing-pulse — engawa-side
certification of the dedup below.

**GREENFIELD (small, bounded — graded honestly, never inflated):** `Cubic::eval`
+ the `Tween`/`Decay`/`Oscillator` samplers + the property-test suite
(fleet-wide absent: `sym:Tween/Timeline/Animator/Spring` = 0 pleme-io evaluators;
the `tweenable.rs` hits are vendored bevy). And `criterion` + `benches/` — the
one genuinely-absent test tool.

---

## 3. The honest non-fit (by construction — the operator's central question)

The reframing from "derive the animation struct" (Layer-A, which
`MACRO-VOCABULARY.md` correctly rejected) to "data-first animation algebra"
(Layer-B) is **half right**. It pays for the keyframe/tween/decay/oscillator
class. It **does not and must not** swallow the two stateful particle sims — an
algebra that does is the false abstraction the doctrine forbids. The non-fit is
enforced **by construction (no form minted for them)**, not by a runtime guard:

- **`SnowState::tick`** (render.rs) — a multi-term stateful integrator: the
  typing-pulse `frame[3]·0.92^(dt·60)` decay **plus** a signed,
  temperature-branched pile/melt accumulation (sign flip at temp=0.5, clamped),
  feeding a GPU uniform. No target, no endpoint. The algebra absorbs **at most**
  the shared `frame_decay` sub-term (§4), never the whole tick.
- **`ScrollKinetics::tick`** — velocity/friction/streak physical integrator, no
  target/duration; a mature, property-tested primitive. Force-fitting *regresses*
  it. (It **is** the Integrator arm conceptually; it is cited, not rewritten.)
- **`AuroraState::tick`** — a degenerate clock-pin (`self.time = elapsed`);
  near-zero leverage, a data point not a fold target.

---

## 4. What shipped this session (M0 vertical slice)

The cleanest real animation, ported end-to-end + the 2-site dedup:

- **`src/motion/`** — the algebra: `Advance` trait, `Curve` (Linear + bézier via
  the `UnitBezier` Newton solve), `Tween`, `Decay` + `frame_decay`, `Oscillator`
  + `blink_on`, `Seconds`/`Unit` `Refined` bounds. Property-tested (dt-invariance,
  endpoints, boundedness, monotonicity, the decay semigroup law, curve-inversion
  convergence).
- **bell-flash port** — `render.rs`'s `bell_flash_frames: u8` (a **frame
  counter** that mis-times at 120 Hz — the flash lasted half as long at double
  refresh) → a dt-based `motion::Tween(peak → 0, 0.2 s, linear)`. This is a
  **real behavioral change at high refresh, named as such** (not laundered into a
  byte-identical claim). It is locked by a **golden byte-pin**: at 60 Hz the
  drawn alpha sequence is *frame-for-frame identical* to the legacy
  `frames/12·peak` decay; the 120 Hz difference is the documented intended delta.
- **`frame_decay` dedup** — the `0.92^(dt·60)` shape hand-rolled at **exactly 2
  sites** (snow typing-pulse + bell glow) collapsed into `motion::frame_decay`.
  Byte-identical; the 2nd copy is the Prime-Directive extract trigger. *Honest:*
  a **2-use** consolidation — it does **not** third-use-prove the broader curve
  algebra (see §7).

---

## 5. Leverage ordering — the algebra is NOT the top win (do not lead with it)

The adversarial pass was blunt and correct: two cheaper wins **dwarf the
algebra's payload** and should lead any perf-focused sequence. Stated honestly so
the plan isn't mis-sold:

1. **Cheapest, shipped-this-session:** the `frame_decay` dedup + (next) the
   **`suggestion_fade` determinism leak** — `render.rs:2706` uses `Instant::now()`
   for an alpha ramp, the **last wall-clock animation**, breaking the dt=0
   determinism model. *Correction folded in:* the fade *curve* is a **linear ramp
   in the external `izumi` crate** (`izumi/src/store.rs`), re-exported via
   `suggest/mod.rs` — so the **only mado-side fix is the `Instant::now()` →
   `ctx.elapsed` leak**; do **not** claim "ease-in → Cubic" (that would require
   editing izumi). This is a determinism fix, not an algebra consumer.
2. **The measured perf win — but split honestly:** `snapshot()` clones a fresh
   `Vec<Vec<Cell>>` every vsync (`render.rs:3024`), self-documented as the
   dominant idle cost. The **persistent-scratch-`Vec` anti-realloc** change can
   land **standalone** (an allocation change, not a damage-gate). The
   **`grid_damage::DirtyRegion` damage-gating proper is ENCUMBERED** — its module
   carries `#![allow(dead_code)] // Consumed at M7`, it is coupled to the deferred
   render-thread decouple that "fights madori `RenderCallback` ownership," and it
   must **not** reintroduce the deliberate idle-full-repaint that dodges the Metal
   swapchain afterimage. So: *stop allocating per idle frame* (shippable) ≠ *skip
   the idle repaint via damage* (sequenced behind M2/M7). Do not conflate them.

**The algebra is the tail, not the head.** It ships correctness (bell 120 Hz) +
a drift-class collapse — real, but modest.

---

## 6. Tier ledger (never round up)

| Component | Tier | Note |
|---|---|---|
| `motion::{Curve, Tween, Decay, Oscillator, Advance, frame_decay}` | **shipped** | this session; property-tested |
| bell-flash → `Tween` port | **shipped** | golden byte-pin @ 60 Hz; 120 Hz = intended delta |
| `frame_decay` 2-site dedup (snow + glow) | **shipped** | byte-identical |
| `Cubic::eval` + arms lifted into `ishou_tokens::motion` | **design** | greenfield-code-in-existing-crate; extract at 3rd consumer |
| `Seconds`/`Unit` bounds via `Refined<f32>` | **only-mitigated** | runtime **clamp**, not a refusal; `Default` bypasses; f32 const-bounds impossible today (UNREP §III.3). NOT parse-rejected — ishou `Refined` deserialize **clamps** |
| `Duration` bound → parse-rejected/truly-unrep | **design** | needs a galho-style `validate`-trait `Refined<Duration>` routing deserialize through `try_new` (const-evaluable case only) |
| dt=0 determinism | **CI-forcing-function** | the `two_identical_renders…` / `thirty_two…` safety net — NOT a compile error |
| `(defeasing)/(deftween)/(defoscillator)/(defanimation)` tatara-lisp forms | **design (M3)** | over-ceremony at M0 for a sole consumer (wireenum precedent); a mado-local parser first (aldrava `spec_lisp.rs` style); engawa's `(defeffect)` proves the shape |
| published `#[derive(TataraDomain)]` motion triplet | **aspirational** | 2nd-fleet-consumer-gated — never speculative |
| criterion perf-gate | **design** | the one absent tool; needs a mado lib-target (bin-only today) or lands at fleet-extraction |
| GPU frame-budget gate (wgpu timestamp queries) | **aspirational** | `LAST_FRAME_US` is CPU-frame-only + self-graded only-mitigated; a budget gate on it alone is a round-up |

---

## 7. Phased path (corrected per the adversarial verdicts)

Lead with the measured/determinism wins; the algebra is the tail. Each phase
independently shippable, byte/golden-pin-gated.

- **M0 — cheap determinism + dedup wins (shipped-in-part).** ✓ `frame_decay`
  helper + snow/glow migration. ☐ `suggestion_fade` `Instant::now()` → `ctx.elapsed`
  (determinism-leak fix only). ☐ first `criterion` bench (needs a lib-target
  decision — see §6). *Gate:* determinism tests stay green.
- **M1 — the anti-realloc snapshot win (standalone).** Persistent scratch
  `Vec<Vec<Cell>>` (clear+refill, never realloc) in `snapshot()` /
  `build_rect_instances` / `build_text_buffers`. **Not** DirtyRegion (encumbered).
  *Gate:* an idle-alloc bench + determinism green.
- **M2 — the algebra's vertical slice (shipped: bell).** ✓ `Tween`/`Curve`/
  `frame_decay` + bell 120 Hz fix. ☐ migrate glow + exit-pulse onto one `Decay`.
  ☐ the local `easing_curves!` table macro (the Layer-B mechanism).
- **M3 — widen + the authoring win.** Cursor/SGR-5 blink → `Oscillator` (pure
  consolidation, *no behavioral win*). The `(def…)` tatara-lisp forms via a
  mado-local parser + shikumi hot-reload + a CATALOG-REFLECTION matrix. **Decide
  TataraDomain-vs-local-parser here, honestly** (keep it open; re-check for a 2nd
  consumer).
- **M4 — model correction + honest ceiling.** Correct the **stale
  `mado/CLAUDE.md`**: Cell is **already ≤ 24 bytes** with `StyleTable`/`LinkTable`
  interning (the "Phase-4 target" **landed**; `size_of::<Cell>() <= 24` passes) —
  the doc's 6-field Cell + "pack codepoint+styleID+flags" is stale. Promote
  `LAST_FRAME_US` to a CI budget gate **labelled CPU-tier**; name the true GPU
  gate (wgpu timestamp queries) **aspirational**.
- **M-aspirational** — lift the sampler to `ishou_tokens::motion`; egaku
  `MotionState` leaf; published `#[derive(TataraDomain)]` triplet — **only** when
  the 3-use rule is met.

---

## 8. The macro vocabulary — honest scope (the operator's explicit ask)

The ask was "a full tatara-lisp/rust/macro-generated vocabulary." The honest
answer, tier-graded:

- **M0/M2 (shipped/next): the Rust typed border + interpreter**, matching mado's
  own shipped `CsiCommand`/`parse_csi_action` triplet tier (border + interpreter,
  **no `.lisp`**), plus a **local `easing_curves!` `macro_rules!` table** (sibling
  of `rect_constructors!`/`dec_private_modes!`) generating the single
  `interpolate(curve, t)` dispatch. This IS the macro-generated vocabulary at the
  M0-appropriate tier.
- **M3 (design): the `(defeasing)/(deftween)/(defoscillator)/(defanimation)`
  tatara-lisp forms** via a mado-local parser — the authoring win. `engawa`'s
  `(defeffect)`/`(defmaterial)` (a `#[derive(DeriveTataraDomain)]` + thin
  `compile()`) is the **proven** shape this mirrors.
- **Deferred (aspirational): a published `#[derive(TataraDomain)]` triplet.**
  Shipping one at M0 for a **sole consumer** is the exact over-ceremony
  `MACRO-VOCABULARY.md` rejects (the `wireenum` precedent). It graduates at the
  2nd fleet consumer, not before. **Categorically not** a per-struct
  `#[derive(Animatable)]` farm derive — heterogeneous state (SnowState alone
  breaks any per-struct derive) is the over-abstraction the doc forbids.

---

## 9. Open risks + stale-model corrections surfaced

- **`mado/CLAUDE.md` is stale** (M4 above): Cell is already ≤24 B (interned);
  `render.rs` is ~9,300 lines (doc says ~2,350); `terminal.rs` ~11,300. Models
  stay current — fix in the M4 pass.
- **Two `ScrollKinetics`** exist: mado's (`src/ux/scroll_kinetics.rs`, ~443 LOC)
  and egaku's (`egaku/src/scroll.rs`, ~923 LOC). A future consolidation candidate,
  not touched here.
- **GPU-conditioned tests:** every golden/determinism test `expect("gpu")` — they
  are shipped + reusable but only on a runner with a GPU adapter; a pure-CPU
  `criterion` interpolate microbench can gate anywhere, the visual gates cannot.
- **DirtyRegion coupling** to the deferred M7 render-thread decouple + the
  deliberate idle-full-repaint — the reason it is *not* pulled forward.

---

*Reference exemplar the whole algebra emanates from:
[`src/ux/scroll_kinetics.rs`](../src/ux/scroll_kinetics.rs) — a typed sub-state
advanced by a pure `tick(dt)` with a strict dt=0 contract and a full property
suite. Motion is that shape, generalized.*
