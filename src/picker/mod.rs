//! The fuzzy-picker substrate — one curated-index + live-fuzzy-search +
//! themed-overlay base that every mado picker is an *expression of*.
//!
//! ## The CSE shape — base + delta
//!
//! Two muscle-memory pickers (Ctrl-S sessions, Ctrl-T directories) used
//! to be ~90% copy-pasted: parallel modal state, parallel render code,
//! parallel overlay-FSM effect sets, four hardcoded Nord colour
//! literals. This module collapses the shared 90% into a typed base and
//! leaves each picker as *just its delta*:
//!
//! | layer | base (here) | per-case delta |
//! |---|---|---|
//! | state | [`state::FuzzyPicker<Row>`] | the `Row` type |
//! | source | [`state::PickerSource`] (`list`) | sessions = praça; dirs = wadachi |
//! | render | [`component::OverlaySpec`] + [`component::OverlayStyle`] (themed) | title, empty-state, anchor |
//! | accept | (engine arm) | dir = `cd` inject; session = switch / create / emoji presets |
//! | freshness | [`reconcile::IndexReconciler`] | live `InProcess` sessions → praça |
//!
//! The render pieces are typed here and drawn by one `draw_overlay`
//! method in [`crate::render`]; the colours resolve from the active
//! [`crate::theme::Theme`] (no Nord literal survives), so a theme swap
//! restyles every picker. The `Row`/source split is extraction-ready —
//! Phase 2 lifts [`state`] + [`reconcile`] + [`presets`] into a fleet
//! crate the other "curated data + live fuzzy search" tools (frost
//! pickers, Ctrl-R history, skim-tab, tend) consume the same way.

pub mod component;
pub mod presets;
pub mod reconcile;
pub mod state;
