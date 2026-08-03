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

use pleme_kindstr_derive::KindStr;
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
    /// `shell`'s argv[1..], handed to the child **as a vector** — never
    /// joined, quoted, or run through `sh -c`. `["-u", "NONE", "f.rs"]`
    /// is three arguments no matter what characters they contain.
    ///
    /// **Honest scope: the EMBEDDED world consumes this; the headless
    /// registry still drops it.** The field was in the advertised schema
    /// from the start, but nothing downstream could carry argv until
    /// tear's `MultiplexerControl` grew an `args` parameter — so both
    /// worlds silently discarded it. The embedded path now carries it
    /// through to execvp; the headless path spawns through mado's own
    /// `Pty::spawn`, whose signature takes a shell + an optional
    /// `sh -c` string and has no argv slot, so closing that half is a
    /// change to the local-PTY spawn contract rather than a wire-up.
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
    /// Which session world to spawn into (session-world union phase 1).
    /// `""`/`"auto"` = spawn into the live GUI's embedded tear registry
    /// when one is reachable (the session shows as a ● row in Ctrl-S),
    /// falling back to this process's headless registry; `"embedded"` =
    /// GUI-only (typed error when unreachable); `"headless"` = the
    /// legacy process-local registry, never forwarded (what tests pin).
    #[serde(default)]
    pub world: String,
    /// Initial column count for the spawned session's grid. `0`
    /// means "inherit from the active window" (windowed spawns) or
    /// "use the standard 80" (headless spawns). Load-bearing for
    /// the headless MCP path — agent-driven debug sessions must be
    /// able to declare grid size up front so layout-sensitive bugs
    /// can be reproduced deterministically.
    #[serde(default)]
    pub cols: u16,
    /// Initial row count for the spawned session's grid. Mirror of
    /// [`Self::cols`] — `0` = "inherit" / "default to 24".
    #[serde(default)]
    pub rows: u16,
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
///
/// The wire-name round-trip ([`Self::from_str_kind`] ↔
/// [`Self::as_str`]) is generated by `#[derive(KindStr)]` (the
/// macro-farm paired round-trip derive) from the per-variant
/// `#[kind(name = "...", alias = "...")]` attrs — one authored
/// placement-name table instead of a hand-written match. The empty
/// string is an `alias` of `tab` so the minimal MCP spec (`{}`)
/// resolves to a tab. [`Placement::Custom`] keeps its bare-ident name
/// (`"Custom"`) and is the catch-all the [`TermSpec::resolved_placement`]
/// wrapper falls back to for any unknown plugin-added string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, KindStr)]
pub enum Placement {
    #[kind(name = "tab", alias = "")]
    Tab,
    #[kind(name = "split-horizontal")]
    SplitHorizontal,
    #[kind(name = "split-vertical")]
    SplitVertical,
    #[kind(name = "window")]
    Window,
    /// A placement the runtime doesn't recognise. Held as-is for
    /// plugin handlers to claim.
    Custom,
}

impl TermSpec {
    /// `window.inherit_working_directory` resolution (M4 stage 2) —
    /// the pure half of "a new session opens where the focused one
    /// is". Precedence, fixed:
    ///
    /// 1. An explicit `cwd` on the spec always wins (the client
    ///    asked for a place; inheritance never overrides intent).
    /// 2. Knob off → unchanged (the knob IS the gate).
    /// 3. Knob on + a focused cwd known (OSC 7) → inherit it.
    /// 4. No focused cwd known → unchanged (falls back to the
    ///    spawn default exactly as before — never an error).
    #[must_use]
    pub fn with_inherited_cwd(
        mut self,
        inherit_enabled: bool,
        focused_cwd: Option<String>,
    ) -> Self {
        if self.cwd.is_empty()
            && inherit_enabled
            && let Some(cwd) = focused_cwd
        {
            self.cwd = cwd;
        }
        self
    }

    /// Resolve the string `:placement` into a typed [`Placement`].
    /// Empty / unknown / "tab" all map to [`Placement::Tab`] via
    /// the canonical table so minimal specs still work. Thin wrapper
    /// over the derived [`Placement::from_str_kind`] — any string the
    /// derive doesn't recognise (a plugin-added placement) falls back
    /// to [`Placement::Custom`] for a plugin handler to claim.
    #[must_use]
    pub fn resolved_placement(&self) -> Placement {
        Placement::from_str_kind(&self.placement).unwrap_or(Placement::Custom)
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
            cols: 0,
            rows: 0,
            world: String::new(),
        }
    }
}

impl TermSpec {
    /// Resolve [`Self::cols`] / [`Self::rows`] into a concrete pair
    /// of dimensions. `0` requests "use the headless default" —
    /// 80×24, the historically-stable terminal size matched by every
    /// VT line discipline. Non-zero values pass through unchanged so
    /// agent-driven tests can declare reproducible grid sizes.
    #[must_use]
    pub fn resolved_dimensions(&self) -> (u16, u16) {
        let cols = if self.cols == 0 { 80 } else { self.cols };
        let rows = if self.rows == 0 { 24 } else { self.rows };
        (cols, rows)
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
    fn inherited_cwd_matrix() {
        // (spec cwd, knob, focused cwd) → resulting cwd. One matrix,
        // every row reported (house aggregation style).
        let rows: &[(&str, bool, Option<&str>, &str, &str)] = &[
            (
                "",
                true,
                Some("/tmp/proj"),
                "/tmp/proj",
                "knob on + focused cwd → inherit",
            ),
            (
                "",
                false,
                Some("/tmp/proj"),
                "",
                "knob off → ignore the focused cwd",
            ),
            (
                "",
                true,
                None,
                "",
                "no focused cwd known → fall back unchanged",
            ),
            ("", false, None, "", "knob off + nothing known → unchanged"),
            (
                "/explicit",
                true,
                Some("/tmp/proj"),
                "/explicit",
                "explicit spec cwd beats inheritance",
            ),
            (
                "/explicit",
                false,
                None,
                "/explicit",
                "explicit spec cwd survives knob off",
            ),
        ];
        let mut failures = Vec::new();
        for (spec_cwd, knob, focused, expected, why) in rows {
            let spec = TermSpec {
                cwd: (*spec_cwd).to_string(),
                ..Default::default()
            };
            let got = spec
                .with_inherited_cwd(*knob, focused.map(str::to_owned))
                .cwd;
            if got != *expected {
                failures.push(format!("{why}: got {got:?}, want {expected:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} inherited-cwd rows failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    #[test]
    fn known_placements_table_has_canonical_entries() {
        for p in ["tab", "split-horizontal", "split-vertical", "window"] {
            assert!(KNOWN_PLACEMENTS.iter().any(|k| *k == p));
        }
    }

    #[test]
    fn placement_round_trips_through_wire_name() {
        // Pin the derived as_str ↔ from_str_kind inverse for every
        // named variant. Custom has no canonical wire name (it's the
        // catch-all the resolver falls back to), so it's excluded —
        // the KNOWN_PLACEMENTS table is the source of named values.
        for name in KNOWN_PLACEMENTS {
            let parsed = Placement::from_str_kind(name)
                .unwrap_or_else(|| panic!("{name} should parse to a placement"));
            assert_eq!(parsed.as_str(), *name, "{name} failed round-trip");
        }
        // The empty-string alias resolves to the same tab variant.
        assert_eq!(Placement::from_str_kind(""), Some(Placement::Tab));
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
