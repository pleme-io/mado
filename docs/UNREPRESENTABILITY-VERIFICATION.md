# Unrepresentability Verification Ledger

Per the org-level ★★ UNREPRESENTABILITY model
(`pleme-io/theory/UNREPRESENTABILITY.md`): **we do not mitigate bad
states — we make them unrepresentable.** This ledger grades every
hardened mado surface honestly, row by row.

## The bright line

- A `Result::Err` / `Option::None` returned at use time is
  **mitigation**, not unrepresentability.
- A **compile error** or an **absent match arm / absent field /
  absent method** is **unrepresentability**.

Three honest tiers, never rounded up:

| Tier | Meaning |
|---|---|
| `truly-unrepresentable` | No expressible program constructs the illegal value — the compiler refuses it (absent arm, absent field, total match, derived projection). |
| `parse-time-rejected` | The only ingress is a boundary that rejects the illegal value before it flows downstream; in-Rust construction is sealed. |
| `only-mitigated` | A runtime guard (clamp, lock, reconcile pass, chokepoint discipline, forcing-function test) sits in front of a state that remains constructible. |

A row graded `only-mitigated` is stated as such — several below are.
`partially` (the org grade) marks a surface between tiers, with the
gap named.

## Ledger

The **Pinned by** column is a mechanical contract:
`tests/unrep_ledger.rs` parses every backticked identifier in that
column and fails the build if the named test function does not exist
in the tree (comment-stripped scan, same style as
`tests/ux_unification.rs`). A ledger row cannot name a test that
rotted away.

| Surface | Illegal state | Technique | Tier (honest) | Pinned by |
|---|---|---|---|---|
| Input modal modes (`ux::modes` Overlay + Pointer FSMs, 2026-06-12) | Search-nav chord consumed with no overlay open (the Esc-eating class, 2026-06-11); both overlays open at once; drag/button-flag desync; forwarded press carrying the shift bypass | Sum-over-product state enums; total `(state, event)` transitions with no wildcard arm on the state enum; `left_button_down` derived, not stored; `ForwardedPress` has no bypass field | `truly-unrepresentable` on the arm/field axis (absent arms, absent field, derived projection); `only-mitigated` on the mirror axis — the renderer-shared `SearchState.active` / `DirPickerState.open` cells still exist as write-only RENDER mirrors (one writer + a pin test, not a type); since the M3 review (2026-06-12) NO engine decision reads them (`reconcile_search` gates on the machine), so a mirror desync blast radius is render-only | `search_nav_arms_exist_only_in_search_state` `pointer_state_event_matrix_holds_invariants` `overlay_state_event_matrix_holds_invariants` `overlay_machine_state_mirrors_shared_cells` |
| Selection content anchors (`terminal::SelectionAnchor`, M-a) | A selection naming evicted / RIS-rebuilt / other-screen content yielding stale coordinates or garbage text | Opaque anchor (sole producer `selection_anchor_at`); resolve-at-use; every read path rejects a dangling endpoint wholesale; per-tick reconciler collapses dangling state | `parse-time-rejected` at the resolution boundary — anchors CAN dangle between eviction and the next read, but no read path yields stale coordinates; the dangling window is unreadable, not absent | `eviction_resolves_selection_to_none` `reset_rejects_pre_reset_anchors` |
| PasteGuard sanitize boundary (`clipboard_store::sanitize_paste`) | Clipboard bytes containing `ESC[201~` escaping bracketed-paste framing and executing as keystrokes | One guarded write chokepoint (`write_paste`) every clipboard delivery routes through; sanitization strips break-out bytes | `only-mitigated` — the chokepoint is discipline plus tests; raw `pty.write` remains callable in-crate and the illegal byte string is constructible | `paste_is_bracketed_and_pasteguard_sanitized` `sanitize_paste_strips_bracket_terminator_when_bracketed` |
| TerminalCaps derived projections (`caps.rs`) | The advertised-caps table drifting from the implemented caps (dual hand-maintained lists) | Projections (`as_pairs`, probe table) derived from one `TerminalCaps` value; matrix test forces every advertised cap to carry a live probe row | `only-mitigated` — coverage is a forcing-function test, not a compile error; a new field without a probe row fails CI, not rustc | `cap_probes_table_covers_every_advertised_cap` `cap_probe_rows_hold_against_the_engine` |
| `scroll_offset` primary-grid clamp (`terminal.rs`) | Viewport offset pointing past scrollback, or output motion dragging the operator's reading position | Runtime clamp (`min(scrollback_len)`) at every mutation site; output pins the view to content; only operator input snaps to the tail | `only-mitigated` — the field is a plain `usize` and the clamp is a runtime guard repeated at mutation sites | `output_while_scrolled_keeps_view_pinned_to_content` `typing_while_scrolled_snaps_view_to_bottom` |
| StyleTable / LinkTable interned ids + gc (`terminal.rs`) | A cell's style/link id pointing at a reaped or re-aliased table entry after gc | Interned ids with gc that remaps every live id in-place; saturation aliases to the last id, never silently to default | `only-mitigated` — ids are bare `u16`s; validity is maintained by the gc walk at runtime, pinned by tests | `style_table_gc_remaps_live_ids_without_default_aliasing` `terminal_gc_preserves_cell_styles` |
| Tear reconciler rendered-truth latch (`InputEngine::on_redraw_tick`) | Grid pushes derived from event-time dims (one frame stale) ping-ponging the pane between old and new grids | `grid_sync_sig` latches on the RENDERED surface signature only; `on_resize` contains no push code at all | `only-mitigated` — the absent push in `on_resize` is convention pinned by a test; nothing in the types stops a future event-time push | `on_resize_defers_to_reconciler_and_push_grid_resizes_both_halves` |
| BoundedFontSize (`font_size.rs`, `ishou_tokens::Refined`) | A rendered font size outside `[FONT_MIN, FONT_MAX]` (the 2026-05-21 runaway-font class) | Newtype whose every construction/mutation path clamps (`Refined<f32, FontSizeBounds>`); key-repeat storms gated upstream | `partially` (org grade for `Refined`) — every mado path clamps so the out-of-range stored value has no local construction path, but a clamp is a construction-time guard, not a rejection, and upstream `Refined::default()` skips it | `inc_step_saturates_at_max` `font_zoom_1000_increments_saturates_at_font_max_in_both_sink_configs` |
| Notification/progress lane split (`ux::side_effects`, M4 stage 1, 2026-06-12) | A ConEmu OSC 9;4 progress update surfacing as a desktop notification (banner spam from a busy progress bar) | Separate typed lanes: progress is `Option<ProgressState>` — its own field on `Terminal` / `TerminalSideEffects` — while the notification queue is `Vec<PendingNotification>`; no constructor maps one type onto the other | `truly-unrepresentable` on the value axis (pushing a `ProgressState` into the notification queue is a type error; the conversion does not exist); `only-mitigated` on the routing axis — the `9;4` parse arm choosing the progress lane is handler discipline, pinned by the matrix, not a compile fact | `conemu_progress_matrix_sets_lane_and_never_notifies` `notification_osc_matrix_enqueues_one_typed_entry_each` |
| Single-drain side-effect ownership (`Terminal::drain_side_effects` + `ux::apply_side_effects`, M4 stage 2, 2026-06-12) | Two per-loop side-effect polling copies drifting apart (the 2026-06-11 hunt class: silent bell, never-updating title, dead OSC 52 in exactly one render mode) | ONE typed producer (the drain — pure state transfer; change-edge title/cwd; rising-edge attention; second immediate drain yields the default payload) + ONE shared consumer both adapters call; per-loop `take_*` / title-diff calls are BANNED tokens in the structural scan | `only-mitigated` — the `take_*` methods remain callable and a third polling copy is constructible; single ownership is enforced by forcing-function tests (drain markers + the determinism pin), not by the type system | `neither_event_loop_contains_direct_ux_logic` `both_call_sites_drive_the_engine` `drain_is_pure_state_transfer_and_second_drain_is_empty` `drain_title_is_a_change_edge_not_a_level` |
| Sixel decode boundary (`Terminal::decode_and_place_sixel`, M3-C3 slice 2, 2026-06-12) | A malformed sixel DCS payload panicking the VT engine, or a decode failure silently dropping bytes with no typed signal | The only ingress is `icy_sixel::SixelImage::decode_from_dcs` returning `Result`; the `Err` arm logs a typed trace and returns — no `unwrap`/`expect`/`todo!`/`panic!` on the payload path; the decoded RGBA flows through the one shared `store_rgba_image` upload path | `only-mitigated` — the rejection is a `Result::Err` at the decode boundary (mitigation by the org bright line, not unrepresentability); the decoder is a sealed third-party crate, so a malformed payload remains constructible but cannot reach the GPU as a half-decoded image | `sixel_payload_decodes_to_expected_dims_and_one_placement` `sixel_malformed_payload_is_rejected_without_panic_or_placement` |

## Tier histogram

| Tier | Rows |
|---|---|
| `truly-unrepresentable` (with a named mitigated axis) | 2 |
| `parse-time-rejected` | 1 |
| `partially` | 1 |
| `only-mitigated` | 7 |

The histogram is the honesty: most of mado's hardening is still
mitigation with forcing-function tests. Each `only-mitigated` row is
either burn-down debt (a typed seam could remove the state) or a
ceiling (external-world observation — e.g. anchor dangling tracks
real content eviction and can only ever be rejected at read time).

## Remediation queue (FIXABLE rows)

- **Overlay mirror cells** — destination: renderer reads the machine
  state through a projection instead of sibling bools
  (`SearchState.active` / `DirPickerState.open` become derived).
- **PasteGuard** — destination: a typed `SanitizedPaste` value whose
  only constructor sanitizes; `write_paste` accepts only that type.
- **StyleTable ids** — destination: generation-tagged ids rejected at
  lookup after gc (parse-time-rejected), if profiling permits.

Ceiling rows (`scroll_offset` clamp against live grid growth, anchor
dangling against content eviction) are runtime-observation bounded —
chasing a compile error there is wasted effort per the org model.
