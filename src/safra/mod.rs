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

// M0 is the typed foundation: its constructors are exercised by the unit tests
// and consumed by the M1 reconcilers + the Ctrl-S projection. Until that wiring
// lands the production paths are unreferenced — allow it here, remove at M1.
#![allow(dead_code)]

pub mod cell;
pub mod config;
pub mod curated;
pub mod gha;
pub mod schema;
pub mod signal;
pub mod source;

pub use cell::SafraCell;
pub use config::{CellTuning, ResolvedTuning, SafraConfig};
pub use curated::{CuratedItem, CuratedSet, Delta, Severity, Tracked};
pub use gha::{GhaDeploymentSource, GhaFilter, GhaRun, TextMatch};
pub use schema::{Endpoint, ServiceKind, TrackedDataKind, TrackedEnvironment};
pub use signal::Signal;
pub use source::{CellSource, ObserveCtx};
