# THEORY.md — The Theory of mado (窓)

> The unifying frame for every system in mado. Read this once; it is the
> lens the per-area docs ([UNREPRESENTABILITY-VERIFICATION](./UNREPRESENTABILITY-VERIFICATION.md),
> [GRID-THREADING-CONTRACT](./GRID-THREADING-CONTRACT.md),
> [GAP-ANALYSIS](./GAP-ANALYSIS.md), [REMEDIATION-PLAN](./REMEDIATION-PLAN.md))
> are read through. It is **tier-honest**: it grades mado's own systems
> destination-vs-shipped and carries a debt→destination ledger (§VIII).
> When a system here says "interim", that is a standing invitation to
> land the destination — not an accepted end state.

---

## I. The thesis — a terminal is a composition of typed primitives

mado is **not bespoke terminal code**. It is an *expression of the
pleme-io fleet substrate*: typed FSMs for interaction, typed effects for
I/O, a typed render graph for pixels, typed design tokens for colour, typed
session orchestration for the multiplexer. The wager (the org's
Constructive Substrate Engineering applied to a GUI app): **if every
recurring shape is a shared typed primitive, behaviour is proven by
construction** — the compiler refuses illegal interaction states, illegal
font sizes, un-linearized colours, and two-overlays-at-once; review reads
type signatures, not implementations; and "add the next feature" becomes
"declare one more instance of a pattern that already exists."

The "janky / hacky" feeling, whenever it appears, is the **absence of the
frame** — a place where we hand-wired what should have been an instance of
a primitive. The cure is always the same: name the primitive, make the
illegal state unrepresentable, and express the feature as `base + delta`.

---

## II. The spine — one typed pipeline, end to end

Every interaction is one flow, and **every arrow is typed**:

```
physical key / mouse / OSC
  │  (madori event)
  ▼
LOWER to mode-independent typed facts            OverlayKey / PressPlan / Action
  │
  ▼
PURE FSM transition  (no I/O, no locks)           Overlay::on_event / Pointer::on_event
  │  → (next state, routing, typed effects)        OverlayStep / PointerStep
  ▼
ENGINE executes effects against typed SEAMS       PtySink · ResizeSink · SessionPickerBridge · cursor_keys_mode
  │  (writes renderer-shared typed mirrors)         Arc<Mutex<{Selection,Search,FuzzyPicker<…>}>>
  ▼
RENDERER reads typed shared state                 TerminalRenderer (+ #[invalidating_setter] deltas)
  │
  ▼
TYPED RENDER PASSES → GPU                          RectInstance (rect) · glyphon (text) · engawa graph (effects)
       ▲                                           colours: ishou Srgb→Linear (never raw, never hex)
       └── design tokens                            ishou_tokens::VellumPalette / SemanticRoles
```

The human authors at the ends (press a key / read the screen); everything
between is mechanically derived from typed values. A new mode, a new
picker, a new effect, a new colour is **a new value in an existing type**,
not a new code path.

---

## III. The core patterns (the data structures + patterns)

These are mado's load-bearing idioms. Every system in §IV is an instance
of one or more.

1. **Typed modal FSM + pure transition + typed effects.** `Overlay` (which
   modal owns the keyboard) and `Pointer` (the drag lifecycle) are enums
   with pure `on_event(state, event) -> (state, routing, effects)`. The
   transition has **no wildcard arm** — a new variant fails to compile
   until every (state, event) pair is decided. I/O lives in *effects* the
   engine executes, never in the FSM. (`src/ux/modes.rs`)

2. **Mechanical exhaustiveness (AllVariants).** Each FSM enum carries a
   `#[derive(pleme_allvariants_derive::AllVariants)]` mirror + `ordinal()`
   total-match; matrix tests enumerate ALL states × ALL events and assert
   invariants, aggregating every failure in one run. The registry size is
   *derived*, never a hand const — a new variant grows it mechanically.

3. **Renderer-shared typed mirrors.** Interaction state the renderer must
   see (`Selection`, `SearchState`, `FuzzyPicker<Row>`) lives in
   `Arc<Mutex<_>>` written **only** by effect executors. The engine's modal
   decisions read the FSM enum, never the mirrors — the mirrors are a
   *projection for rendering*, not a second source of truth. (See §VI for
   where this principle is not yet fully honoured.)

4. **Trait seams for divergence.** The two runtime modes (local PTY,
   embedded tear) differ only at typed seams: `PtySink`, `ResizeSink`,
   `cursor_keys_mode()`, `SessionPickerBridge`. The engine is
   mode-agnostic; a seam being `None` is also how a feature is *gated*
   (no bridge ⇒ inert session switcher). (`src/ux/sinks.rs`, `engine.rs`)

5. **Base + delta substrate.** A family of similar things is ONE generic
   base + per-member deltas. The pickers are the reference: one
   `FuzzyPicker<Row>` + `PickerSource` (the base), and each picker is just
   its `Row` + accept delta (session = switch/create/presets; dir = `cd`
   inject). (`src/picker/`)

6. **Invalidating-setter delta render.** Renderer fields carry
   `#[invalidating_setter]` (`pleme-invalidating-setter-derive`); a setter
   flips an invalidation bit so the next frame re-emits only stale
   primitives. No hand-written setters, no cache desync.

7. **Typed colour emission through ishou.** Every colour that reaches the
   GPU is linearized through `ishou_tokens::Srgb::to_linear()`; every
   *content* colour is an ishou token. Raw sRGB and hand-hex are banned
   from the GPU path (the washed-out-retina class is unrepresentable).
   (`src/theme.rs`) — see §VII.

8. **Typed render graph; composition is the source of truth.** Effects are
   an `engawa` graph; the enabled set AND the per-frame params both derive
   from ONE `AmbienceComposition` value, so they cannot drift. The perf
   governor scales a *quality word*, never the composition. (`src/ambience.rs`,
   `render_graph.rs`)

9. **Refined<T, Bounds> type-level invariants.** `BoundedFontSize =
   Refined<f32, FontSizeBounds>` makes an out-of-range font size
   unconstructible — saturation by type, not by a runtime clamp at each
   call site. (`src/font_size.rs`)

10. **Single application point.** `apply_config_theme` is the ONE place
    theme reaches both the renderer and the mirror `Terminal` — one
    source, two sinks, so the two render modes cannot diverge.

11. **Time injection.** praça + the reconciler take `now: u64`; mado owns
    the single clock-read. Ranking + reconciliation stay deterministic and
    testable. (`auto_attach.rs`, `picker/reconcile.rs`)

12. **Typed boundary results.** `EventOutcome` maps totally to
    `madori::EventResponse` (no `..Default` spread) — adding a field breaks
    the build until every site decides it. (`src/ux/outcome.rs`)

---

## IV. The subsystem catalog — every system that made it so far

| System | What it is | Built on | Core pattern(s) |
|---|---|---|---|
| **Interaction FSMs** | `Overlay` + `Pointer` machines | — | §III.1, §III.2 |
| **Input engine** | mode-agnostic owner of all interaction state | seams | §III.3, §III.4, §III.12 |
| **Keybind / action** | chord→`Action` atlas, `KindStr` round-trip, `KeyRepeatGate` | awase, `pleme-kindstr-derive` | typed dispatch + rate-limit |
| **Action injection** | kanshou (MCP) → action queue → same dispatch path | kanshou | §III.4 |
| **Selection** | content-anchored span FSM, dangle-reconcile | `SelectionAnchor` | §III.1, late-bound anchors |
| **Search** | absolute-row match list + grid scan | Terminal | mirror + re-scan-on-resize |
| **Clipboard / paste** | content-addressed store + `PasteGuard` + image→PNG | hasami | parse-don't-validate (sanitize) |
| **Picker substrate** | `FuzzyPicker<Row>` + `PickerSource` + `OverlaySpec` | praça, wadachi, ishou | §III.5 |
| **Session orchestration** | praça index + auto-attach-on-cd + reconciler | praça, ishou | §III.11, base+delta |
| **tear integration** | embedded `InProcess`, switch channel, engate stream | tear-core/-types | §III.4 |
| **Render pipeline** | clear → rects → text → effects → overlays | madori, garasu, glyphon | §III.6, three-pass split |
| **Theming** | `Theme` + `apply_config_theme`; Vellum from BORN ishou | ishou-tokens, irodzuki | §III.7, §III.10 |
| **Effects** | aurora/bloom/grain/glow/snow as one composition | engawa, engawa-wgpu | §III.8 |
| **Fonts / glyph** | `BoundedFontSize`, symbol routing, shape cache | ishou `Refined`, shikumi | §III.9 |
| **Kanshou MCP** | live GUI introspection + control over a socket | kanshou, shidou | seam to the agent plane |
| **Vigy** | optional in-process tatara-lisp reconciler | vigy | continuous convergence |
| **Config** | typed `MadoConfig`, hot-reload | shikumi | typed config (fleet rule) |
| **Caps / env** | truecolor/terminfo projection into `SpawnEnv` | caps | typed env projection |
| **Notifications** | BEL / OSC 9/777/99 dispatch | tsuuchi | typed notification |

mado consumes **~29 pleme-io git substrate crates**. The terminal is the
*integration surface* of the fleet's GPU + design + session + attestation
primitives — which is exactly the point: the value is in the substrate,
and mado is one of its faces.

---

## V. The invariants mado proves (by construction or near it)

- **At most one modal owns the keyboard** — the `Overlay` enum (truly-unrep
  at the FSM; the *render* projection is only-mitigated today — §VI).
- **Font size is always in `[6, 64]`** — `Refined<f32, FontSizeBounds>`
  (truly-unrep: no out-of-range value constructs).
- **Button state never desyncs from the drag** — `left_button_down()` is
  *derived* from `Pointer`, not a sibling bool (truly-unrep).
- **A paste cannot forge bracketed-paste framing** — `PasteGuard` strips
  the terminators at the boundary (parse-time-rejected).
- **Theme cannot diverge across render modes** — one application point
  (structural).
- **No un-linearized colour reaches the GPU** — the ishou Srgb→Linear path
  is the only ingress (structural in the render path).
- **A new FSM/event/outcome variant cannot ship half-wired** — no-wildcard
  matches + total `From` impls + AllVariants matrices (compile-time).

---

## VI. The single-source-of-truth principle — and the overlay debt

The deepest pattern is §III.3: **the renderer reads a projection, the FSM
is the truth.** mado does not yet fully honour it in one place, and that
place is the canonical worked example of the principle.

**The debt.** The renderer reads **three independent `.open`/`.active`
mirror cells** (search / dir-picker / session-picker). The `Overlay` FSM
guarantees exactly one owns the keyboard, but three independent bools can,
under a mirror/FSM desync, both read `true` — and the renderer would draw
**two overlays at once** (the operator-observed "centred session popup
*and* a top-left dir picker", 2026-06-21). `modes.rs` documents this as
*only-mitigated*: every `Open*` emits sibling-`Close` effects first.

**The interim (shipped).** A render-layer **single-overlay priority gate**
(session > dir > search) draws at most one overlay regardless of mirror
state. Correct as an invariant, but it disambiguates by a *priority
heuristic*, not by the truth.

**The destination.** The renderer reads **one** typed value — the FSM's
`Overlay` — and matches on it to draw exactly the modal that owns the
keyboard. The per-picker `.open` bool stops being a render gate; "two
overlays visible" becomes *unrepresentable*, not gated. This promotes the
overlay axis from `only-mitigated → truly-unrepresentable` and is the
**#1 structural item** in §VIII.

---

## VII. The ishou-compliance principle — all visual is a token

**Every colour mado shows is an ishou token, never a hand-authored hex.**
For the fleet theme (Vellum) this is ~95% true today: background,
foreground, cursor, selection, the search band, and the agent accent all
flow from `VellumPalette::vellum().surfaces()` + `SemanticRoles::vellum()`
+ the `content_ansi_16()` keystone, so a retune in ishou propagates on the
next compile with zero mado-local hex.

**The gap.** The Ctrl-S popup *card* (panel fill / border / selected-row
bar) is currently derived in mado via a local `blend()` of existing tokens
(`background`↔`foreground`, `background`↔`agent_accent`). It *consumes*
ishou tokens (compliant in spirit) but does the elevation math locally.
**The destination** is first-class ishou popup-surface tokens
(`popup_panel` / `popup_border` / `popup_selected_bg` on the Vellum
surfaces) that mado reads directly — no local colour math, retunable in
ishou. Until then the `blend()` is marked interim (§VIII). The irodzuki
presets (nord/dracula) have no popup surfaces; they derive from the scheme
— acceptable, as those are a foreign palette system, not ishou-native.

---

## VIII. The debt → destination ledger (tier-honest)

Standing rule: **every mado PR advances a row's tier or leaves a typed
`pending-mado: <row>` note.** A row marked *interim* is a remediation
item, never an accepted end.

| # | Current (shipped) | Tier | Destination | Pattern |
|---|---|---|---|---|
| 1 | three `.open` render mirrors; single-overlay priority gate | only-mitigated | renderer reads ONE `Overlay` value; double-draw unrepresentable | §III.3, §VI |
| 2 | two parallel 7-variant picker effect sets + hand engine dispatch | works, duplicated | `#[derive(PickerOverlay)]` emits the effect variants + FSM arms + dispatch; a new picker = one declaration | §III.5, EMITTER SUBSTRATE |
| 3 | popup card colours via mado-local `blend()` | ishou-in-spirit | first-class ishou `popup_*` surface tokens read directly | §III.7, §VII |
| 4 | Center panel reuses the cell-rect instance buffer | works, coupled | a dedicated overlay rect batch (own buffer/lifetime) | §III.6 |
| 5 | `create_and_switch` builds a `SessionRecord` by patching the pub `name_seed` field | works, ad-hoc | a typed praça constructor (`for_preset(idx)` / `for_named(name)`) | typed construction |
| 6 | reconciler runs on picker-open (`bridge.refresh`) | covers the case | a continuous tick keeps the index live always (optional; open-sync is sufficient for the picker) | §III.11 |
| 7 | search-status uses `agent_accent`; popup text roles share `OverlayStyle` | fine | a fuller modal-chrome token set if modal sophistication grows | §III.7 |

Cross-references: the unrepresentability tiers + the per-pattern verdicts
live in [UNREPRESENTABILITY-VERIFICATION.md](./UNREPRESENTABILITY-VERIFICATION.md);
the broader remediation order in [REMEDIATION-PLAN.md](./REMEDIATION-PLAN.md).

---

## IX. How to extend mado (the operating rules)

- **A new interaction mode** is a new `Overlay`/`Pointer` variant — the
  no-wildcard match forces you to decide every transition; the AllVariants
  matrix forces a test row.
- **A new picker / fuzzy surface** is a `PickerSource` + a `RowKind`
  delta over `FuzzyPicker<Row>`, drawn through `draw_overlay` — never a new
  overlay copy. (Ctrl-R history, completions, command palette: all this.)
- **A new visual** is an ishou token consumed through `Theme` /
  `OverlayStyle`, never a hex literal; if the token doesn't exist, add it
  to ishou, then consume it.
- **A new effect** is an entry in the `AmbienceComposition`, not a bespoke
  pass.
- **A new mode divergence** (local vs tear vs future remote) is a trait
  seam, not an `if mode`.
- **A new bounded scalar** (zoom, opacity, scrollback) is a
  `Refined<T, Bounds>`, not a runtime clamp.
- **A new agent-facing capability** is a kanshou MCP tool that drives the
  SAME typed `Action` / effect path a human key takes.

---

## X. Lineage + naming

mado (**窓**, "window") sits in the fleet's GPU-app lineage beside
namimado, escriba, hibiki — all on `garasu` (GPU substrate), `madori`
(event loop), `ishou` (design tokens), `engawa` (effects), `awase`
(input), `hasami` (clipboard/VTE). Its session plane is `praça` (naming +
attach) over `tear` (multiplexer); its agent plane is `kanshou` (MCP) +
`vigy` (reconcilers); its config is `shikumi`; its emission discipline is
the fleet's TYPED EMISSION + UNREPRESENTABILITY laws. Foundational crates
take Japanese names; newer Tier-2 primitives take Brazilian-Portuguese
names — mado is a *consumer-and-integrator* of both registers, which is
why a "theory of mado" is largely a theory of *how the fleet's primitives
compose into one running window.*

---

*This document is canonical for mado's architecture. When a system here
changes, update the row in §IV/§VIII that names it — a stale theory
actively misleads (Compounding Directive #4).*
