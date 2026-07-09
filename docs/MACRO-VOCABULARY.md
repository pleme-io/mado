# Mado Macro Vocabulary — the generate-don't-author destination

> **★★ EMITTER SUBSTRATE + Pillar 12 (generation over composition), applied to
> mado.** This is the destination-first plan (Operating Principle #0) for
> maximizing *generated* code across mado: every recurring impl-shape and every
> typed problem-space table becomes an authored declaration a macro expands,
> not a hand-kept match. Companion to [`GAP-ANALYSIS.md`](./GAP-ANALYSIS.md) /
> [`REMEDIATION-PLAN.md`](./REMEDIATION-PLAN.md). Authored 2026-07-09.

## The destination (named first)

**mado = one typed VT algebra + operator-authored data domains, sitting on the
macro farm, with zero hand-written mechanical impl-blocks.** Two layers:

- **Layer A — impl-shape derives.** Every enum↔slug/byte table, Copy getter,
  `matches!()` predicate, consuming-self builder, and cache-invalidating setter
  is a macro-farm derive (`KindStr` / `KindByte` / `AllVariants` / `GetterAll` /
  `IsVariant` / `WithBuilder` / `InvalidatingSetter`), never a hand match. mado
  is already the fleet's most macro-mature terminal (5 farm derives live:
  `KindStr`, `KindByte`, `AllVariants`, `InvalidatingSetter`, `FleetThemed`), so
  most of Layer A is *finishing adoption*, not new abstraction.

- **Layer B — the problem-space VT vocabulary (the real leverage).** One
  params-carrying wire-enum spec family — `(defdecmode)`, `(defosc)`, a promoted
  `(defcsi)`, `(defsgr)` — from which **both** the inbound parse-dispatch **and**
  the outbound wire-emit are mechanically derived (the TYPED-SPEC + INTERPRETER
  triplet mado already proved with `CsiCommand` / `parse_csi_action` in
  `vt.rs`). One new farm derive underpins it: **`pleme-wireenum-derive`**
  (`parse(&[u32]) -> Option<Self>` + `emit(&self) -> Vec<u8>` from one
  per-variant `#[wire(...)]` table — the parse↔emit symmetry no existing derive
  covers; its emit body **must** route through `vt.rs`).

## The macro farm — what mado consumes / can consume

`tatara-rust-ast` (`catalogs/pleme-derives.lisp`) publishes ~20 derives. Exact
contracts of the ones this plan touches:

| Derive | Generates | Requires | Attrs |
|---|---|---|---|
| `KindStr` | `as_str(&self) -> &'static str` + `from_str_kind(&str) -> Option<Self>` | unit variants | `#[kind(name=…, alias=…)]` (default name = ident) |
| `KindByte` | `KindStr` pair **+** `as_byte(&self) -> u8` + `from_byte(u8) -> Option<Self>` | unit variants; `byte` on **every** variant | `#[kind(name=…, alias=…, byte=N)]` |
| `AllVariants` | `pub const ALL: &[Self]` + `pub const fn all()` | unit variants | — |
| `GetterAll` | per-field `pub fn <field>(&self) -> &<T>` | named struct | — |
| `IsVariant` | per-variant `is_<variant>(&self) -> bool` | enum | — |
| `WithBuilder` | per-field `with_<field>(mut self, v) -> Self` | named struct | — |
| `InvalidatingSetter` | `set_<field>` that also invalidates a cache field | named struct | `#[invalidating_setter]` field opt-in |

A **new** derive lands as one `catalog.json`/`pleme-derives.lisp` entry →
`tatara-rust-forge catalog-instantiate` emits + verifies + publishes the crate
(★★ EMITTER SUBSTRATE: author the Spec, not the proc-macro; publish upstream,
*then* consume — never inline a derive in mado).

## The phased path (each independently shippable, byte-pin-gated)

| M | What | Risk | ~LOC | Status |
|---|---|---|---|---|
| **M0** | Adopt `KindStr`/`AllVariants` on the verified non-adoption enums (`Subject`, `ServiceKind`) | low | ~50 | **✓ shipped** (nix-green) |
| **M1** | Generic per-field/per-variant derives (Getter/Builder/IsVariant/InvalidatingSetter-extend) | low | ~0 | **✗ assessed — no clean fit** (see the learning) |
| **M2** | `ImplFrom` on by-value projections | med | ~0 | **✗ assessed — arm-by-arm enum maps ≠ newtype From** |
| **M3** | `pleme-kindmirror-derive` (ux/ FSM twin + total `kind()` + `ordinal()`, 4 sites) upstream, then consume | med | ~120 | **deferred** — 4 sites are `#[cfg(test)]` scaffolding (not production drift); a local `mirror_enum!` or the upstream derive is the future step when a 2nd fleet consumer appears (3-use rule) |
| **M4** | `dec_private_modes!` — one table generates `dec_set`/`dec_reset`/DECRQM + **fixed the mode-12 drift bug** | med | ~70 | **✓ shipped** |
| **M5** | typed `vt::osc_color_reply`/`osc4_color_reply` — **killed the 2 `format!("\x1b]…")` emission violations** | low | ~28 | **✓ shipped** |
| **M6** | The VT emit vocabulary (parse⇆emit) | med | ~30 | **✓ shipped (typed-emission sweep)** — routed the value-composing VT replies (OSC 22 pointer-shape, XTWINOPS text-area size, DECRQSS SGR/DECSCL/DECSCA/invalid) through the typed `vt::osc`/`vt::csi`/`vt::dcs` emitters, byte-pinned (`typed_replies_are_byte_identical…`). **`pleme-wireenum-derive` + `CsiCommand::to_bytes()` rejected** — the `vt` emitters already *are* the typed emit vocabulary, re-emitting inbound `CsiCommand`s has no consumer (sole-consumer crate = over-engineering, per the core learning), and forcing the SGR parse table onto the hot path is banned. Isolated fixed constants left as-is (the rule blesses them) |
| **M7** | SGR flag + underline parse⇆emit | high | — | **✓ done (coherence-locked, hot-path untouched)** — flags: `AttrFlags::ALL` table + parse coherence lock; **underline**: two emit⇆reparse **fixpoint** tests (`underline_style_emit_reparse_fixpoint`, `underline_color_emit_reparse_fixpoint`) bind `handle_sgr` ⇆ `SgrReport::Display` for `UnderlineStyle`/`UnderlineColor`. **fg/bg left alone** (correct lossy asymmetry — parse resolves index→RGB, emit is one `38:2` write; a round-trip test there would assert a *false* invariant). A 4-agent adversarial design confirmed: zero production/hot-path change, no runtime table, no derive. **Engawa follow-up (specified, deferred):** lift the 4:N + 58 wire codes onto engawa's owned `UnderlineStyle`/`UnderlineColor` as `const fn` helpers (completing the `AttrFlags::ALL` single-ownership pattern) — real solve-once but cross-repo, off the hot path, and the drift risk is *already* closed by the two fixpoint tests |
| **M8** | `deftheme`/`defkeybind` TataraDomain + config `TieredConstructor` | med | ~180 | **deferred** — themes are already declarative via irodzuki presets (low value; only the one Nord/Vellum path is worth lifting); `TieredConstructor` is a new farm derive (future) |
| **M9** | MCP body `macro_rules!` **feeding rmcp** | med | ~200 | **deferred** — rmcp already macro-registers dispatch+schema (a bespoke derive would fork the transport); a local body-template `macro_rules!` is moderate value (future) |

### The live drift bug M4 fixes (found during analysis)

`terminal.rs`: DECRQM (`dec_rqm`) already knows DEC mode **12** (cursor blink),
but `dec_set` (4742) and `dec_reset` (4775) do **not** — three hand-kept copies
of one code→field registry that have already drifted. `(defdecmode)` collapses
all three into one table; the mode-12 arm flowing consistently through set/reset
is the **one intentional behaviour delta** in the whole plan (proven by a
caps-honesty probe, never absorbed into a "byte-identical" claim).

## ★ The core learning — "maximize macros" ≠ "derive everything"

The single most important finding of this refactor, and the reason the tier is
honest. Macro leverage is **not uniform** — it splits cleanly in two, and only
one half pays:

1. **Domain / problem-space tables (Layer B) — the real prize.** Where the code
   is *the same algorithm transcribed N times against a table of cases* — the VT
   dispatch (`dec_set`/`dec_reset`/DECRQM = one DEC-mode registry three times),
   the OSC/CSI/SGR wire grammar (parse ⇆ emit) — one authored table + a generated
   interpreter **eliminates a whole drift class** (M4 literally found + fixed a
   live drift bug: mode 12 lived in only one of the three copies). This is where
   "a macro vocabulary for the problem space" earns its keep. Mechanism: a local
   `macro_rules!` table, or a `#[derive(TataraDomain)]` `(def…)` form, or the
   TYPED-SPEC triplet — authored once, generating parse + emit + report.

2. **Genuine impl-shape duplication (Layer A, narrow).** Where a *real*
   hand-kept table already exists — an enum's `slug()`/`ALL`/`from_slug` match
   (M0: `Subject`, `ServiceKind`) — an existing farm derive (`KindStr`/
   `AllVariants`/`KindByte`) collapses it. This pays **only where the hand table
   is real**; adopting on an enum that has no such table just *adds* speculative
   API.

**What does NOT pay — and adopting it is over-abstraction (debt, forbidden):**
blanket per-field/per-variant deriving of getters/setters/builders/predicates.
Every generic farm derive verified here (`GetterAll`, `WithBuilder`,
`InvalidatingSetter`-extend, `IsVariant`) was a **non-fit** for mado because
they emit *all* fields/variants with a *fixed, direct-assign, by-reference*
shape, while mado's hand methods are **ergonomic and specific** — by-value `Copy`
getters (`cols() -> usize`, not `-> &usize`), `impl Into<String>` + `Some(…)`
wrapping builders, 2-field atomic invalidating setters, computed predicates
(`is_up = amount > 0`, not a variant match). Force-fitting them would change
signatures, collide with existing methods, expose internals, add unused API, and
**regress ergonomics** — the opposite of the goal. A mature codebase's remaining
hand methods are hand-written *because the generic shape doesn't fit*.

**The rule this yields:** author a macro/derive for the **actual recurring
shape** (M3 `kindmirror`, M6 `wireenum` are *new* derives designed to fit mado's
real patterns — so they *will* fit), and reach for an *existing* derive only
where the hand table it replaces genuinely exists. "Maximize generated code" =
**generate the mechanical table/dispatch once; keep the ergonomic surface
hand-crafted.** Never derive for the sake of a derive count.

## Verification strategy (mado's own three idioms, carried forward)

1. **Byte-pin tests** — the `vt.rs::csi_matches_the_legacy_format_strings`
   idiom: every codegen'd output (enum slugs, DECRQM/OSC replies,
   `CsiCommand::to_bytes`, SGR effects) is asserted **byte-identical to the
   pre-refactor literal** (diff the old match/`format!()` into the assertion
   *before* refactoring), not merely self-consistent.
2. **Tier/dead-knob config invariants** — the `config.rs *_tiers`
   yaml-round-trip tests are the byte-verify harness for the M8
   `TieredConstructor` derive.
3. **FleetThemed drift test** — `ishou_tokens::convergence::Guard::for_app("mado")`
   pins theme/font convergence; extended in M8.
4. **Derive-crate snapshot tests** — new farm derives (`kindmirror`, `wireenum`)
   verified on the farm side via `quote!→syn::parse2→prettyplease` +
   `assert_tokens_contain!` (never raw-substring), emit bodies proven to route
   through `vt.rs`.
5. **Forcing-function fixtures** — M3 ships a compile-fail fixture proving a new
   payload variant without a `kind()` arm fails to compile (`E0004`).

`cargo build && cargo test` green is the close-gate for every milestone. (Note:
mado's build **tolerates `dead_code` warnings** — it ships dozens of unused
methods today — so an unused derived paired-inverse is not a build blocker, but
new *speculative* surface is still avoided per the rejections below.)

## Honest rejections (over-abstraction is debt too)

Do **not** re-propose these — each was evaluated and rejected:

- **`pleme-bitflags-derive`** (`CellAttrs`/`AttrFlags`) — `AttrFlags` carries a
  load-bearing `(Self, &str)` SGR-code registry cross-referenced by
  `SgrReport`, and `CellAttrs` is a frozen legacy wire-projection marked "never
  grow it" — **not** a plain bitset. The upstream `bitflags` crate already owns
  plain bitsets. Reject unless a derive demonstrably beats `bitflags` *and*
  preserves the registry.
- **`DockProgress` / `SuppressReason` / `Severity` for M0** — `DockProgress` has
  a `Percent(u8)` payload variant (can't take unit-only derives); `SuppressReason`
  has no hand-written table to collapse (adoption = speculative API);
  `Severity::weight` (1/4/16) is a rank ladder, not a wire byte (`KindByte` drags
  in 3 unused methods). Kept hand-written.
- **`JanitorKind` slug for M0** — only a 2-arm `slug()` and no parse direction;
  too small to justify `KindStr`'s paired inverse. (Its `ALL` is test-only with
  `#[allow(dead_code)]`, which a derived `pub const ALL` can't carry.)
- **`GetterAll` on `Terminal` / `TerminalRenderer`** (was M1's bulk) — the
  catalog `GetterAll` emits `pub fn <field>(&self) -> &<field_ty>` for **every**
  field, **by reference, with no skip attribute**. mado's getters are
  **by-value Copy** (`cols(&self) -> usize`, not `-> &usize`), the methods
  **already exist** (deriving would be a duplicate-definition error), and it
  would expose every private field. Verified non-fit. A viable version needs a
  *new* `CopyGetter` farm derive (field opt-in, returns `T` for `Copy` fields) —
  a new-derive decision, not a mechanical adoption. Rejected for M1 as-specified.
- **GPU animation-state structs** (`SnowState`/`GlowState`/`AuroraState`) —
  `tick()` bodies differ materially; fails the third-use test. Inline.
- **Per-theme constructor fns** — already declarative via irodzuki base16
  presets (`theme_from_scheme`); only the one hand-written Nord/Vellum path is
  worth lifting (M8), and `ThemeSpec::prescribed_default` **stays Nord** (pinned
  by recent commits).
- **`set_font_size` / `set_scale_factor`** — recompute cell metrics; genuine
  derived-metric logic, not plain setters.
- **A bespoke `defmcptool` derive** — rmcp's `#[tool]`/`#[tool_router]` already
  generate dispatch + schema; a new proc-macro would fork the transport. M9 uses
  mado-local `macro_rules!` that *feed* rmcp's existing scan.
- **`defmadoconfig`** (whole config schema → Rust + HM/NixOS/Darwin Nix trio) —
  named as destination but **deferred**: high-risk / long-horizon. The M8
  `TieredConstructor`/`DefaultKnob` derives are the honest interim.
