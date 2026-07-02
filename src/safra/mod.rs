//! Safra — the observability-curation mini-framework.
//!
//! *safra* (Brazilian-Portuguese: a **harvest / yield**) — the standing harvest
//! of signals each reconcile cycle gathers. Safra maintains long-term, curated,
//! in-memory structures — one per tracked data-type per tracked environment —
//! and feeds them efficiently into the Ctrl-S suggestion stream. Vigy agents
//! rebuild the harvest on every mado startup, then converge it to upstream
//! deltas in batch.
//!
//! Canonical spec: `docs/SAFRA.md`. Built on the existing `crate::suggest`
//! substrate (`SuggestionSource` / `SuggestionEnvironment` / the persisted store
//! / the `vigy` runtime) + the ~10 observability sources already shipped.
//!
//! Off by default (the bare tier ships no endpoints, no secrets, no pooling);
//! the `blackmatter-akeyless` config layer flips it on and supplies the three
//! tuning scopes.
//!
//! M0 (this module): the typed core — the declared schema ([`schema`]), the
//! long-term curated set + batch convergence ([`curated`]), and the three-scope
//! tunable config ([`config`]). M1+ land the tatara-lisp domains, the source
//! matrix, the blackmatter-akeyless layer, and convergence hardening (see
//! `docs/SAFRA.md` §10).

// The plane is LIVE via the `SafraSuggestionSource` adapter (registered in the
// suggestion engine; cells from the `safra:` config section). A handful of
// typed-API items (catalog reflection surfaces, M1 lisp-authoring hooks) are
// still exercised only by tests — allow dead_code module-wide until the M1
// authoring surface consumes them rather than scatter per-item allows.
#![allow(dead_code)]

pub mod adapter;
pub mod cell;
pub mod config;
pub mod curated;
pub mod gha;
pub mod project;
pub mod schema;
pub mod signal;
pub mod source;
pub mod vm_alerts;

// The safra facade. The binary consumes `SafraSuggestionSource` + `SafraConfig`
// directly; the rest of the surface is the M1 lisp-authoring + catalog API,
// consumed cross-module under cfg(test) or via full paths — same idiom as the
// suggest facade.
#[allow(unused_imports)]
pub use adapter::SafraSuggestionSource;
#[allow(unused_imports)]
pub use cell::SafraCell;
#[allow(unused_imports)]
pub use config::{CellTuning, ResolvedTuning, SafraConfig};
#[allow(unused_imports)]
pub use curated::{CuratedItem, CuratedSet, Delta, Severity, Tracked};
#[allow(unused_imports)]
pub use gha::{GhaDeploymentSource, GhaFilter, GhaRun, TextMatch};
#[allow(unused_imports)]
pub use schema::{Endpoint, ServiceKind, TrackedDataKind, TrackedEnvironment};
#[allow(unused_imports)]
pub use signal::Signal;
#[allow(unused_imports)]
pub use source::{CellSource, ObserveCtx};
#[allow(unused_imports)]
pub use vm_alerts::VmAlertsSource;
