# Responsive-by-default GUI + the unified Rust widget library

> **Direction (operator, 2026-07-09):** mado GUI refreshes — the Ctrl-S board
> especially — must be **screen-size-aware and reflow as the screen changes,
> by default, for anything**. Leverage macro generation for a *tested widget
> vocabulary*, and leverage pleme-io to **colocate/redistribute one full-Rust
> widget library** that plugs into terminal + web + mobile (all Rust → unify
> windowing/GUI). This doc is the destination-first plan (recon'd against what
> already exists — extend egaku/ishou/madori/engawa, never rebuild).

## Destination (three layers, one warp)

**(a) Responsive-by-default as a TYPED INVARIANT.** Every UI element is sized
against a live viewport, and "omitted screen-awareness" is *unrepresentable* —
a drawer cannot be called without a resolved viewport-derived budget in its
type. Keystone types: a madori `Viewport { physical, scale, logical }` on
`AppEvent::Resized`; ishou fluid/breakpoint tokens resolving to `Refined<T,B>`;
a layout algebra (`VisibleRows`, `LayoutSolve→RectTree`, a cross-face `MinSize`
gate lifting QUADRO P9 from terminal-only).

**(b) The macro-generated tested widget vocabulary.** `(defwidget)` /
`(deftoken)` / `(defkeymap)` TataraDomain forms (TYPED-SPEC triplet over the
existing `pleme-widget-spec` border) → **`pleme-widget-gen` folded into teia**
→ **one spec generates per-face renderers**: TUI (egaku-term `draw::<name>`),
GPU (egaku→garasu), web (Leptos). Widget *logic* (the `(state,event)→(state,
effects)` FSM + `view()` snapshot — FuzzyPicker's proven shape) stays
hand-crafted in **egaku**; only the mechanical per-face *renderer* + the
responsive-layout projection are generated. **This is the core macro-vocabulary
learning applied** (see [`MACRO-VOCABULARY.md`](./MACRO-VOCABULARY.md)):
generate the domain/layout tables, keep the ergonomic FSM hand-crafted, don't
over-abstract. Verified by a re-render-diff CI matrix.

**(c) The one redistributable full-Rust widget library.** **egaku IS the
library** (v0.1.3, 142 tests — ListView/TabBar/SplitPane/TextInput/FuzzyPicker/
Modal/FocusManager), joined to every ecosystem at three seams: ishou (tokens) +
awase (keymaps) + the WidgetSpec typescape. It plugs into terminal (egaku-term
v0.3.0 — **shipped**: typed Cell/Buffer, diff renderer, TestBackend, no
`format!()`), native-GPU (egaku→garasu — the aspirational half to build), and
web (Leptos via teia). Distributed via AUTO-RELEASE; consumers config-only.

## Tier-honest gap (the plan is loud about it)

(b) and (c) are **largely unshipped**: `pleme-widget-gen` is an **empty repo**
(0 commits — a schema, not a compiler); **no `(defwidget)`/`(deftoken)` surface
exists**; WidgetSpec has **zero consumers**; teia hardcodes Leptos and doesn't
consume WidgetSpec; the **egaku→garasu GPU renderer doesn't exist**. M4/M5 are
high-risk multi-session builds. **The first slice (M0) depends on none of it.**
Also **stale org model to fix** (models-stay-current): org CLAUDE.md's
`pending-quadro:2` grades egaku-term "v0.2.1 full-clears + `format!()`" — it is
**v0.3.0** now (typed Cell/Buffer + diff + TestBackend + no `format!()`,
2026-07-06); re-grade it.

## Phased path

| M | What | Risk | Status |
|---|---|---|---|
| **M0** | Responsive Ctrl-S board + Ctrl-T picker in mado — delete the fixed `WINDOW_ROWS=12`, derive the visible cap from the live surface as a typed `VisibleRows`; seal it as an invariant | low | **✓ shipped** |
| **M0.1** | Horizontal analog of M0: cap each overlay LINE's width to the live window (`max_overlay_content_w`), truncating the string with a visible ellipsis before shaping rather than letting the panel/GPU scissor clip it silently — the "Ctrl-S doesn't resize appropriately" report (long suggestion-stream rows ran off the right edge). No config surface existed for this at all before; the fix is unconditional (same posture as M0: a sealed invariant, not an opt-in) | low | **✓ shipped** |
| **M1** | madori `Viewport { physical, scale, logical }` on `AppEvent::Resized` + re-emit the swallowed `ScaleFactorChanged` (mirror `ScrollDelta::{Lines,Pixels}`) | low | queued |
| **M2** | ishou `Breakpoint`/`Fluid { min, preferred, max }` token tier → `resolve(viewport) -> Refined<T,B>` | low | queued |
| **M3** | A new leaf layout crate (consumes ishou tokens + a madori Viewport) — pure `LayoutSolve → RectTree` + a cross-face `MinSize` gate; widgets take a resolved `Layout`, never raw pixels (responsive becomes structurally unrepresentable-to-omit) | med | queued |
| **M4** | `(defwidget)` + `pleme-widget-gen` folded into teia — one spec emits egaku-term + garasu + Leptos renderers; re-render-diff CI gate | high | queued |
| **M5** | Unify the library — build egaku→garasu GPU renderer; collapse the 3-way token fork (egaku Nord / pleme-mui Md3 / ishou) onto ishou; mado's pickers become generated egaku widgets | high | queued |

## M0 — shipped (the first slice)

`src/row_budget.rs` — `VisibleRows = Refined<usize, RowBudgetBounds>` (the fleet
`ishou_tokens::Refined` primitive, same shape as `BoundedFontSize`). The **only**
in-draw-path constructor is `RowBudget::for_viewport(height, line_h, pad, pad_y)`
— the same vertical-fit formula `draw_overlay` clamps its window to — so **a
picker sized without the current screen is unrepresentable**. `render.rs`'s
`overlay_row_budget(height)` resolves it per frame (at the reconciler tick that
already reconciles the grid → tracks resize with zero new event wiring); the
Ctrl-S board (`draw_session_picker`) and Ctrl-T picker (`draw_dir_picker`) now
build as many rows as the live surface affords instead of a fixed 12. The
reserved-band **anchor** stays a documented constant (`ROWS_DEFAULT`) — it's a
screen-less union-ordering policy, not a viewport-fit concern (smallest change
that closes the *real* gap; no hot-path/memo risk). Tests: `budget_grows_with_
height`, `budget_is_refined_clamped_at_the_floor`, `budget_saturates_at_the_
ceiling`, `matches_draw_overlay_max_lines_formula`.

## M0.1 — shipped (the horizontal half)

`render.rs`'s `draw_overlay` computed `content_w`/`vis_max_w()` straight from
each line's *shaped* text width, uncapped against the live window — a long
board row (a suggestion-stream alert, a long path) could size the panel wider
than the window, and the excess just ran off-screen with no ellipsis, clipped
by whatever bounds the GPU pass. `max_overlay_content_w(width, pad, pad_x)` is
the width analog of `max_lines`'s height clamp; `truncate_overlay_text(text,
highlights, max_chars)` truncates char-boundary-safe (multibyte-safe, drops
highlight positions past the cut) and appends a visible ellipsis before the
line is ever shaped, so `vis_max_w()` can no longer exceed the ceiling — the
panel is sealed to the window on both axes now, unconditionally (no config
toggle; every overlay through `draw_overlay` gets it, Ctrl-S and Ctrl-T
alike). Tests: `overlay_content_w_shrinks_with_window_width`, `overlay_
content_w_never_goes_negative_on_a_tiny_window`, `truncate_overlay_text_is_a_
noop_when_it_already_fits`, `truncate_overlay_text_shortens_and_ellipsizes`,
`truncate_overlay_text_drops_highlights_past_the_cut`, `truncate_overlay_
text_handles_zero_budget_without_panic`, `truncate_overlay_text_is_multibyte_
safe`.

## Reuse map (extend, never rebuild)

- **resize** → extend **madori** (typed `Viewport`); don't re-derive per consumer.
- **overlay geometry** → reuse mado's single `draw_overlay` (already screen-
  derived centring + `max_lines`) + the pure `viewport_line_window` /
  `centered_panel_geom`; new sizing goes *below* the `OverlaySpec` seam.
- **clamp** → reuse `ishou_tokens::Refined<T,B>`; extend ishou with `Fluid`/
  `Breakpoint`.
- **widget library** → reuse **egaku** (the dual-use widget typescape) + its
  FuzzyPicker `(state,event)→(state,effects)` total-match FSM as the template;
  reuse **egaku-term v0.3.0** for the TUI renderer.
- **macro surface** → reuse `pleme-widget-spec` (the pure-type border) — make it
  a *compiler* (fold pleme-widget-gen into **teia**), don't re-author it.
- **keys** → reuse **awase** `BindingMap` + `KeyRepeatGate` unchanged.
- **engawa** → leave it a pure GPU-pass DAG; **layout is a new leaf crate**, not
  flexbox nodes bolted onto the render-graph IR.
