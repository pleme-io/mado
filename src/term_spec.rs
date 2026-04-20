//! Typed spec for "spawn a terminal session from a declaration".
//!
//! The shared contract between mado and escriba (or anything else
//! speaking mado's MCP). Every cross-process "open a terminal with
//! these properties" request flows through this struct; its JSON
//! schema is what the MCP tool advertises to clients.
//!
//! Escriba will eventually ship a `defterm` tatara-lisp form that
//! serializes to the same JSON shape, so `escriba --rc …` can
//! drive mado over MCP without either side negotiating an ad-hoc
//! protocol. This module is the source of truth for that shape.

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Everything needed to spawn a terminal session in mado. Every
/// field has a sensible default so clients can send the smallest
/// useful request (empty object = "open a new tab with the user's
/// default shell in the current cwd").
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TermSpec {
    /// Shell command to run. `bash` / `zsh` / `fish` / `frost` or a
    /// full path. Empty = use the user's `$SHELL`.
    #[serde(default)]
    pub shell: String,
    /// Extra args passed to the shell.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory. `~` expands to `$HOME`. Empty = inherit
    /// mado's cwd.
    #[serde(default)]
    pub cwd: String,
    /// Env vars merged onto the spawn environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Session title surfaced in the tab bar and `list_sessions`
    /// output. Empty = derive from shell / cwd at spawn time.
    #[serde(default)]
    pub title: String,
    /// Where to place the new session. One of [`KNOWN_PLACEMENTS`].
    /// Empty string is accepted and resolved to [`Placement::Tab`]
    /// so the minimal request is schema-valid.
    #[serde(default)]
    pub placement: String,
    /// Session id to attach to instead of spawning. When non-empty,
    /// `shell` / `args` / `cwd` / `env` are ignored — this is the
    /// "focus an existing session" path.
    #[serde(default)]
    pub attach: String,
    /// Shader effects to activate for this session only — names
    /// mirror escriba's `defeffect :name` canonical set
    /// (`cursor-glow`, `bloom`, `scanlines`, …). Empty = fleet
    /// defaults.
    #[serde(default)]
    pub effects: Vec<String>,
}

/// Canonical placement values — where the new session opens.
/// Exposed for clients (escriba, MCP inspector) that want to
/// enumerate valid `:placement` strings without cracking the
/// JSON-Schema.
#[allow(dead_code)] // Consumed by test assertions + future MCP describe tool.
pub const KNOWN_PLACEMENTS: &[&str] = &[
    "tab",              // new tab in the active window (default).
    "split-horizontal", // horizontal split of the active pane.
    "split-vertical",   // vertical split of the active pane.
    "window",           // new top-level window.
];

/// Typed placement. `TermSpec::placement` is a string on the wire
/// (so the JSON-Schema stays open to plugin-added placements) but
/// the handler resolves to one of these before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Tab,
    SplitHorizontal,
    SplitVertical,
    Window,
    /// A placement the runtime doesn't recognise. Held as-is for
    /// plugin handlers to claim.
    Custom,
}

impl TermSpec {
    /// Resolve the string `:placement` into a typed [`Placement`].
    /// Empty / unknown / "tab" all map to [`Placement::Tab`] via
    /// the canonical table so minimal specs still work.
    #[must_use]
    pub fn resolved_placement(&self) -> Placement {
        match self.placement.as_str() {
            "" | "tab" => Placement::Tab,
            "split-horizontal" => Placement::SplitHorizontal,
            "split-vertical" => Placement::SplitVertical,
            "window" => Placement::Window,
            _ => Placement::Custom,
        }
    }

    /// True when this spec asks to attach to an existing session
    /// rather than spawn a new one.
    #[must_use]
    pub fn is_attach(&self) -> bool {
        !self.attach.is_empty()
    }

    /// Best-effort human-readable title. Falls back to the shell
    /// name, then cwd basename, then `"mado"`.
    #[must_use]
    pub fn display_title(&self) -> String {
        if !self.title.is_empty() {
            return self.title.clone();
        }
        if !self.shell.is_empty() {
            if let Some(name) = std::path::Path::new(&self.shell)
                .file_name()
                .and_then(|n| n.to_str())
            {
                return name.to_string();
            }
        }
        if !self.cwd.is_empty() {
            if let Some(base) = std::path::Path::new(&self.cwd)
                .file_name()
                .and_then(|n| n.to_str())
            {
                return base.to_string();
            }
        }
        "mado".to_string()
    }
}

impl Default for TermSpec {
    fn default() -> Self {
        Self {
            shell: String::new(),
            args: Vec::new(),
            cwd: String::new(),
            env: HashMap::new(),
            title: String::new(),
            placement: String::new(),
            attach: String::new(),
            effects: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_spec_resolves_to_sensible_defaults() {
        let s = TermSpec::default();
        assert_eq!(s.resolved_placement(), Placement::Tab);
        assert!(!s.is_attach());
        assert_eq!(s.display_title(), "mado");
    }

    #[test]
    fn placement_table_accepts_all_canonicals() {
        let mut s = TermSpec::default();
        for (literal, expected) in [
            ("", Placement::Tab),
            ("tab", Placement::Tab),
            ("split-horizontal", Placement::SplitHorizontal),
            ("split-vertical", Placement::SplitVertical),
            ("window", Placement::Window),
            ("plugin-cascade", Placement::Custom),
        ] {
            literal.clone_into(&mut s.placement);
            assert_eq!(s.resolved_placement(), expected, "placement {literal}");
        }
    }

    #[test]
    fn attach_wins_over_shell() {
        let s = TermSpec {
            shell: "bash".into(),
            attach: "pane-42".into(),
            ..Default::default()
        };
        assert!(s.is_attach());
    }

    #[test]
    fn display_title_falls_back_through_shell_cwd_mado() {
        let s = TermSpec {
            shell: "/usr/bin/frost".into(),
            ..Default::default()
        };
        assert_eq!(s.display_title(), "frost");

        let s = TermSpec {
            cwd: "/Users/me/code/blog".into(),
            ..Default::default()
        };
        assert_eq!(s.display_title(), "blog");

        let s = TermSpec {
            title: "ship-rust".into(),
            ..Default::default()
        };
        assert_eq!(s.display_title(), "ship-rust");
    }

    #[test]
    fn known_placements_table_has_canonical_entries() {
        for p in ["tab", "split-horizontal", "split-vertical", "window"] {
            assert!(KNOWN_PLACEMENTS.iter().any(|k| *k == p));
        }
    }

    #[test]
    fn spec_round_trips_through_json() {
        let original = TermSpec {
            shell: "zsh".into(),
            args: vec!["-l".into()],
            cwd: "~/code".into(),
            title: "dev".into(),
            placement: "split-vertical".into(),
            effects: vec!["cursor-glow".into(), "bloom".into()],
            ..Default::default()
        };
        let wire = serde_json::to_string(&original).unwrap();
        let parsed: TermSpec = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed.shell, original.shell);
        assert_eq!(parsed.args, original.args);
        assert_eq!(parsed.placement, original.placement);
        assert_eq!(parsed.effects, original.effects);
        assert_eq!(parsed.resolved_placement(), Placement::SplitVertical);
    }

    #[test]
    fn minimal_json_deserializes_to_default() {
        // The smallest useful MCP payload: `{}`. Every field
        // defaults so clients don't have to know the full schema.
        let parsed: TermSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.resolved_placement(), Placement::Tab);
        assert!(parsed.args.is_empty());
        assert!(parsed.env.is_empty());
    }
}
