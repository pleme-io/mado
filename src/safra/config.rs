//! Three-scope tunable config (global / group / specific), resolved most-
//! specific-wins per field. See `docs/SAFRA.md` §6. Bare tier = off entirely;
//! the `blackmatter-akeyless` layer supplies the on-config on top.

use std::collections::HashMap;

/// A partial set of knobs at one scope. Every field is optional so an unset knob
/// falls through to the next-broader scope (specific → group → global).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CellTuning {
    /// Reconcile cadence in seconds (how often vigy re-pools this cell).
    pub cadence_secs: Option<u64>,
    /// Max curated items kept for this cell (memory insurance; 0 = unbounded).
    pub max_items: Option<usize>,
    /// Override the cell on/off independent of the global master switch.
    pub enabled: Option<bool>,
    /// Per-cell upstream-call budget as a fraction of the endpoint's rate limit
    /// (the samba `quotaPct` knob), 0.0–1.0.
    pub quota_pct: Option<f64>,
}

impl CellTuning {
    /// Overlay `more_specific` onto `self`, taking each set field from the more
    /// specific scope. Returns the merged tuning.
    #[must_use]
    fn overlay(&self, more_specific: &CellTuning) -> CellTuning {
        CellTuning {
            cadence_secs: more_specific.cadence_secs.or(self.cadence_secs),
            max_items: more_specific.max_items.or(self.max_items),
            enabled: more_specific.enabled.or(self.enabled),
            quota_pct: more_specific.quota_pct.or(self.quota_pct),
        }
    }
}

/// The fully-resolved knobs for one (kind × env) cell — every field concrete,
/// backfilled from defaults. This is what a reconciler actually runs on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedTuning {
    pub cadence_secs: u64,
    pub max_items: usize,
    pub enabled: bool,
    pub quota_pct: f64,
}

impl ResolvedTuning {
    /// Hardcoded last-resort defaults (used when neither global nor any scope
    /// sets a field). Gentle cadence, bounded memory, full single-consumer quota.
    const FALLBACK: ResolvedTuning = ResolvedTuning {
        cadence_secs: 120,
        max_items: 50,
        enabled: true,
        quota_pct: 1.0,
    };

    fn from_tuning(t: &CellTuning) -> ResolvedTuning {
        ResolvedTuning {
            cadence_secs: t.cadence_secs.unwrap_or(Self::FALLBACK.cadence_secs),
            max_items: t.max_items.unwrap_or(Self::FALLBACK.max_items),
            enabled: t.enabled.unwrap_or(Self::FALLBACK.enabled),
            quota_pct: t.quota_pct.unwrap_or(Self::FALLBACK.quota_pct),
        }
    }
}

/// The safra config surface. `enabled = false` is the bare tier (no pooling, no
/// endpoints, no secrets); the blackmatter-akeyless layer flips it on and
/// supplies the three scopes.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SafraConfig {
    /// Master switch. Bare = `false`.
    pub enabled: bool,
    /// Scope 1 — defaults for every (kind × env) cell.
    pub global: CellTuning,
    /// Scope 2 — per group name (a data-kind group OR an environment group;
    /// both resolve from the same map, applied in declaration order).
    pub groups: HashMap<String, CellTuning>,
    /// Scope 3 — per specific cell, keyed `"<kind>@<env>"` (most specific).
    pub cells: HashMap<String, CellTuning>,
}

impl SafraConfig {
    /// The canonical specific-cell key.
    #[must_use]
    pub fn cell_key(kind: &str, env: &str) -> String {
        let mut k = String::with_capacity(kind.len() + 1 + env.len());
        k.push_str(kind);
        k.push('@');
        k.push_str(env);
        k
    }

    /// Resolve the effective tuning for a (kind × env) cell, layering
    /// global ⊕ every matching group ⊕ the specific cell — most specific wins
    /// per field. `kind_groups`/`env_groups` are the cell's group memberships.
    #[must_use]
    pub fn resolve(
        &self,
        kind: &str,
        kind_groups: &[String],
        env: &str,
        env_groups: &[String],
    ) -> ResolvedTuning {
        let mut merged = self.global.clone();
        // Group scope: apply every group the cell belongs to (kind + env
        // groups), in a stable order so resolution is deterministic.
        for g in kind_groups.iter().chain(env_groups.iter()) {
            if let Some(gt) = self.groups.get(g) {
                merged = merged.overlay(gt);
            }
        }
        // Specific scope wins last.
        if let Some(ct) = self.cells.get(&Self::cell_key(kind, env)) {
            merged = merged.overlay(ct);
        }
        ResolvedTuning::from_tuning(&merged)
    }

    /// Whether a cell runs: the master switch AND the cell's resolved `enabled`.
    #[must_use]
    pub fn cell_enabled(
        &self,
        kind: &str,
        kind_groups: &[String],
        env: &str,
        env_groups: &[String],
    ) -> bool {
        self.enabled && self.resolve(kind, kind_groups, env, env_groups).enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(cadence: Option<u64>) -> CellTuning {
        CellTuning { cadence_secs: cadence, ..Default::default() }
    }

    #[test]
    fn bare_default_is_off() {
        let c = SafraConfig::default();
        assert!(!c.enabled, "safra ships off in the bare tier");
    }

    #[test]
    fn global_applies_when_no_group_or_cell() {
        let mut c = SafraConfig { enabled: true, ..Default::default() };
        c.global = t(Some(300));
        let r = c.resolve("alerts", &[], "rio", &[]);
        assert_eq!(r.cadence_secs, 300);
    }

    #[test]
    fn group_overrides_global_specific_overrides_group() {
        let mut c = SafraConfig { enabled: true, ..Default::default() };
        c.global = t(Some(300));
        c.groups.insert("prod".into(), t(Some(60)));
        c.cells.insert(SafraConfig::cell_key("oom", "prod-euw1"), t(Some(30)));

        // env in group "prod", no specific cell → group wins over global.
        let g = c.resolve("oom", &[], "prod-use2", &["prod".into()]);
        assert_eq!(g.cadence_secs, 60, "group beats global");

        // specific cell present → beats group AND global.
        let s = c.resolve("oom", &[], "prod-euw1", &["prod".into()]);
        assert_eq!(s.cadence_secs, 30, "specific beats group");
    }

    #[test]
    fn unset_fields_fall_through_per_field() {
        // group sets cadence only; max_items must still come from global.
        let mut c = SafraConfig { enabled: true, ..Default::default() };
        c.global = CellTuning { cadence_secs: Some(300), max_items: Some(99), ..Default::default() };
        c.groups.insert("prod".into(), t(Some(60)));
        let r = c.resolve("oom", &[], "p1", &["prod".into()]);
        assert_eq!(r.cadence_secs, 60, "group's cadence");
        assert_eq!(r.max_items, 99, "global's max_items falls through");
    }

    #[test]
    fn kind_group_and_env_group_both_apply() {
        let mut c = SafraConfig { enabled: true, ..Default::default() };
        c.groups.insert("critical".into(), CellTuning { quota_pct: Some(1.0), ..Default::default() });
        c.groups.insert("prod".into(), t(Some(45)));
        let r = c.resolve("oom", &["critical".into()], "p1", &["prod".into()]);
        assert_eq!(r.cadence_secs, 45, "env-group cadence applied");
        assert!((r.quota_pct - 1.0).abs() < f64::EPSILON, "kind-group quota applied");
    }

    #[test]
    fn cell_enabled_gated_on_master_switch() {
        let mut c = SafraConfig { enabled: false, ..Default::default() };
        c.global = CellTuning { enabled: Some(true), ..Default::default() };
        assert!(!c.cell_enabled("a", &[], "e", &[]), "master off → cell off");
        c.enabled = true;
        assert!(c.cell_enabled("a", &[], "e", &[]), "master on + cell on → runs");
    }

    #[test]
    fn config_round_trips_through_yaml() {
        let mut c = SafraConfig { enabled: true, ..Default::default() };
        c.global = t(Some(120));
        c.groups.insert("prod".into(), t(Some(60)));
        let y = serde_yaml_ng::to_string(&c).unwrap();
        let back: SafraConfig = serde_yaml_ng::from_str(&y).unwrap();
        assert_eq!(c, back);
    }
}
