//! Projection — a curated [`Signal`] → a Ctrl-S board [`Suggestion`] carrying a
//! scoped-session [`SpawnSpec`]. This is what makes "target an item → open a
//! bound session → work it" real on the board (see `docs/SAFRA.md` §1.5).
//!
//! The projection is pure + parameterized by the owning `SourceKind` (supplied
//! by the `SuggestionSource` adapter that reads the curated store — the
//! integration that registers a safra source in the live stream). It maps:
//! severity → rank, the signal's `<env>:<kind>:<identity>` signature → a stable
//! suggestion id (so the SAME item keeps ONE id across refreshes, riding the
//! store's dedup + shade-in continuity), and the signal into a pre-named session.

use std::path::Path;

use super::curated::{CuratedItem, Severity};
use super::signal::Signal;
use crate::suggest::core::{Rank, SourceKind, SpawnSpec, Suggestion};

/// Cap the session-name label so the picker row stays readable.
const NAME_LABEL_MAX: usize = 40;

/// Severity → the board rank. Critical errors top the board; warnings (live
/// deploys, in-band breaches) sit high; info (fresh successes) sits low.
fn severity_rank(sev: Severity) -> Rank {
    match sev {
        Severity::Critical => Rank::critical(),
        Severity::Warning => Rank::high(),
        Severity::Info => Rank::low(),
    }
}

/// A concise, always-non-empty session name scoped to the signal:
/// `<env>: <label…>` (env is always present, so the name can't be empty —
/// `SpawnSpec::new` therefore always succeeds for a real cwd).
fn session_name(sig: &Signal) -> String {
    let mut n = String::with_capacity(sig.env.len() + 2 + sig.label.len().min(NAME_LABEL_MAX));
    n.push_str(&sig.env);
    if !sig.label.is_empty() {
        n.push_str(": ");
        push_truncated(&mut n, &sig.label, NAME_LABEL_MAX);
    }
    n
}

fn push_truncated(out: &mut String, s: &str, max: usize) {
    if s.chars().count() <= max {
        out.push_str(s);
        return;
    }
    for c in s.chars().take(max.saturating_sub(1)) {
        out.push(c);
    }
    out.push('…');
}

/// Project one curated signal into a board [`Suggestion`]: a pre-named session
/// scoped to the item (working dir `cwd`), titled by the signal, ranked by
/// severity, with `env` + detail + deep-link as secondary context. The
/// suggestion id is derived from the signal's stable signature, so it survives
/// refreshes. `cwd` is the session's working directory (the operator's code root
/// or a per-lane dir). Returns `None` only if `cwd` is empty (unreachable for a
/// real board context).
#[must_use]
pub fn project_signal(sig: &Signal, source: SourceKind, cwd: &Path) -> Option<Suggestion> {
    // Attach the on-call prewarm strategy (kube-context + best-effort
    // pod-describe + open the deep-link) so choosing this alert's session lands
    // the operator already-in-the-issue. Empty for a signal that yields no
    // steps — then it's a bare scoped session, as before.
    let spawn = SpawnSpec::new(cwd, session_name(sig))?.with_prewarm(super::prewarm::prewarm_for_signal(sig));
    let key = sig.signature();
    let title = if sig.label.is_empty() { sig.env.clone() } else { sig.label.clone() };

    let mut detail = String::from("env ");
    detail.push_str(&sig.env);
    if let Some(d) = sig.detail.as_deref() {
        detail.push_str(" · ");
        detail.push_str(d);
    }
    if let Some(l) = sig.link.as_deref() {
        detail.push_str(" · ");
        detail.push_str(l);
    }

    Some(
        Suggestion::new(source, &key, title, spawn)
            .ranked(severity_rank(sig.severity))
            .detail(detail),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::core::Urgency;

    // The real owning kind — the `SafraSuggestionSource` adapter supplies it
    // in production; the projection itself stays source-kind-agnostic.
    const STUB: SourceKind = SourceKind::Safra;

    fn sig() -> Signal {
        Signal::new("mte_production", "deploy-flows", "o/r#42", Severity::Critical, "Deploy auth → mte production")
            .with_link("https://github.com/o/r/actions/runs/42")
    }

    #[test]
    fn projects_a_clickable_scoped_suggestion() {
        let s = project_signal(&sig(), STUB, Path::new("/code")).expect("projected");
        assert_eq!(s.title, "Deploy auth → mte production");
        assert_eq!(s.urgency, Urgency::Critical, "critical severity → critical urgency");
        assert!(s.spawn.name().starts_with("mte_production"), "session scoped to the env");
        assert_eq!(s.spawn.cwd(), Path::new("/code"));
        assert!(s.detail.as_deref().unwrap().contains("env mte_production"));
        assert!(s.detail.as_deref().unwrap().contains("actions/runs/42"), "deep-link in detail");
    }

    #[test]
    fn id_is_stable_across_refreshes() {
        let a = project_signal(&sig(), STUB, Path::new("/code")).unwrap();
        let b = project_signal(&sig(), STUB, Path::new("/code")).unwrap();
        assert_eq!(a.id, b.id, "same signal signature → same suggestion id across refreshes");
    }

    #[test]
    fn severity_maps_to_board_rank() {
        let warn = Signal::new("e", "k", "i", Severity::Warning, "x");
        let info = Signal::new("e", "k", "i", Severity::Info, "x");
        assert_eq!(project_signal(&warn, STUB, Path::new("/c")).unwrap().urgency, Urgency::High);
        assert_eq!(project_signal(&info, STUB, Path::new("/c")).unwrap().urgency, Urgency::Low);
    }

    #[test]
    fn long_label_is_truncated_in_the_session_name() {
        let long = "x".repeat(100);
        let s = Signal::new("env", "k", "i", Severity::Info, &long);
        let proj = project_signal(&s, STUB, Path::new("/c")).unwrap();
        assert!(proj.spawn.name().chars().count() <= "env: ".len() + NAME_LABEL_MAX);
        assert!(proj.spawn.name().ends_with('…'));
    }
}
