//! M4 stage 2 — watched-config delta-apply.
//!
//! The shikumi watcher publishes config edits; THIS module is what
//! applies them. The shape is FSM-style per the determinism
//! directive: [`diff`] is a pure function from `(old, new)` configs
//! to a typed [`SetterCall`] effects list, and the executor walks
//! that list against a [`ConfigSetters`] target (production: the
//! [`TerminalRenderer`]; tests: a counting double). Same `(old, new)`
//! pair → same call list, byte for byte; identical configs → an
//! empty list, so a reload that changes nothing touches nothing.
//!
//! Both render loops (`main.rs` local-PTY and `gui_tear_attach`)
//! poll ONE [`ConfigHotReload`] per frame — the shared consumer per
//! the M1/M4 unification pattern (`tests/ux_unification.rs` requires
//! the `.poll_config_reload(` seam in both adapters). The watch
//! callback itself is reduced to a dirty flag; the live config is
//! always read back from the shikumi store, never smuggled through
//! the callback.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::{CursorStyle, MadoConfig, MadoEffectsConfig};
use crate::render::TerminalRenderer;
use crate::terminal::Color;
use crate::theme::Theme;

/// One renderer mutation the config diff decided is needed. The
/// variant set mirrors — exactly — the live `set_*` surface on
/// [`TerminalRenderer`] that boot-time config application uses;
/// a config axis with no setter here is a config axis hot-reload
/// cannot change (window geometry, shell, scrollback: spawn-time).
#[derive(Debug, Clone, PartialEq)]
pub enum SetterCall {
    /// Theme ANSI-16 palette → `set_ansi_colors`.
    AnsiColors([Color; 16]),
    /// Theme selection overlay (linearized RGBA) → `set_selection_bg`.
    SelectionBg([f32; 4]),
    /// Theme cursor color (linearized RGBA) → `set_cursor_color`.
    CursorColor([f32; 4]),
    /// Background clear color + default text color → `set_bg_fg`.
    BgFg {
        /// Linearized clear color (theme background or
        /// `appearance.background`, with `appearance.opacity`).
        bg: wgpu::Color,
        /// Default foreground (theme or `appearance.foreground`).
        fg: Color,
    },
    /// Effective font size (`font_size × accessibility.font_scale`)
    /// → `set_font_size`.
    FontSize(f32),
    /// `cursor.style` → `set_cursor_style`.
    CursorStyle(CursorStyle),
    /// `cursor.blink && !reduce_motion` → `set_cursor_blink`.
    CursorBlink(bool),
    /// `cursor.blink_rate_ms` → `set_cursor_blink_rate_ms`.
    CursorBlinkRateMs(u32),
    /// `window.padding` (logical px) → `set_padding`.
    Padding(f32),
    /// `appearance.bold_is_bright` → `set_bold_is_bright`.
    BoldIsBright(bool),
    /// `accessibility.reduce_motion` → `set_reduce_motion`. Always
    /// ordered BEFORE [`SetterCall::Effects`], matching
    /// `apply_effects_and_accessibility`.
    ReduceMotion(bool),
    /// `MadoConfig::resolved_effects()` — the single colorblind /
    /// alias resolution point — → `set_effects_config` (the single
    /// effects ingress; M3 contract preserved).
    Effects(MadoEffectsConfig),
}

/// The executor seam: every setter the config diff can decide to
/// call. [`TerminalRenderer`] implements it by delegation to its
/// inherent setters; tests implement it with a counting double so
/// "`apply_delta(x, x)` calls ZERO setters" is pinned mechanically.
pub trait ConfigSetters {
    fn set_ansi_colors(&mut self, v: [Color; 16]);
    fn set_selection_bg(&mut self, v: [f32; 4]);
    fn set_cursor_color(&mut self, v: [f32; 4]);
    fn set_bg_fg(&mut self, bg: wgpu::Color, fg: Color);
    fn set_font_size(&mut self, v: f32);
    fn set_cursor_style(&mut self, v: CursorStyle);
    fn set_cursor_blink(&mut self, v: bool);
    fn set_cursor_blink_rate_ms(&mut self, v: u32);
    fn set_padding(&mut self, v: f32);
    fn set_bold_is_bright(&mut self, v: bool);
    fn set_reduce_motion(&mut self, v: bool);
    fn set_effects_config(&mut self, v: MadoEffectsConfig);
}

impl ConfigSetters for TerminalRenderer {
    // Inherent methods win name resolution over trait methods, so
    // each line delegates to the real (derive-generated or
    // hand-written) setter — no recursion is expressible here.
    fn set_ansi_colors(&mut self, v: [Color; 16]) { TerminalRenderer::set_ansi_colors(self, v); }
    fn set_selection_bg(&mut self, v: [f32; 4]) { TerminalRenderer::set_selection_bg(self, v); }
    fn set_cursor_color(&mut self, v: [f32; 4]) { TerminalRenderer::set_cursor_color(self, v); }
    fn set_bg_fg(&mut self, bg: wgpu::Color, fg: Color) { TerminalRenderer::set_bg_fg(self, bg, fg); }
    fn set_font_size(&mut self, v: f32) { TerminalRenderer::set_font_size(self, v); }
    fn set_cursor_style(&mut self, v: CursorStyle) { TerminalRenderer::set_cursor_style(self, v); }
    fn set_cursor_blink(&mut self, v: bool) { TerminalRenderer::set_cursor_blink(self, v); }
    fn set_cursor_blink_rate_ms(&mut self, v: u32) { TerminalRenderer::set_cursor_blink_rate_ms(self, v); }
    fn set_padding(&mut self, v: f32) { TerminalRenderer::set_padding(self, v); }
    fn set_bold_is_bright(&mut self, v: bool) { TerminalRenderer::set_bold_is_bright(self, v); }
    fn set_reduce_motion(&mut self, v: bool) { TerminalRenderer::set_reduce_motion(self, v); }
    fn set_effects_config(&mut self, v: MadoEffectsConfig) { TerminalRenderer::set_effects_config(self, v); }
}

/// The render-facing values a config resolves to — the same
/// resolution the boot path performs in `main.rs` (theme lookup,
/// sRGB→linear with opacity, font-scale multiply, blink gated by
/// reduce-motion, alias-resolved effects). The diff compares THESE,
/// not raw config fields, so e.g. renaming `theme: nord` to an
/// unknown name (which boot would ignore) emits no color calls.
struct Resolved {
    /// `None` when the theme name resolves to no preset — boot
    /// leaves the renderer's current palette in place, so the diff
    /// emits nothing for these three either.
    ansi: Option<[Color; 16]>,
    selection_bg: Option<[f32; 4]>,
    cursor_color: Option<[f32; 4]>,
    bg: wgpu::Color,
    fg: Color,
    font_size: f32,
    cursor_style: CursorStyle,
    cursor_blink: bool,
    cursor_blink_rate_ms: u32,
    padding: f32,
    bold_is_bright: bool,
    reduce_motion: bool,
    effects: MadoEffectsConfig,
}

// `u32 → f32` padding: operator padding is single-digit logical px;
// precision loss is unrepresentable in practice and the boot path
// performs the identical cast.
#[allow(clippy::cast_precision_loss)]
fn resolve(config: &MadoConfig) -> Resolved {
    let theme = Theme::by_name(&config.theme);
    let bg_srgb = match theme {
        Some(t) => ishou_tokens::Srgb::new(t.background.r, t.background.g, t.background.b),
        None => ishou_tokens::Srgb::from_hex(&config.appearance.background)
            .unwrap_or(ishou_tokens::Srgb::new(0x2e, 0x34, 0x40)),
    };
    let bg: wgpu::Color = bg_srgb
        .to_linear()
        .with_alpha(config.appearance.opacity)
        .into();
    let fg = match theme {
        Some(t) => t.foreground,
        None => {
            let fg_srgb = ishou_tokens::Srgb::from_hex(&config.appearance.foreground)
                .unwrap_or(ishou_tokens::Srgb::new(0xec, 0xef, 0xf4));
            Color::new(fg_srgb.r, fg_srgb.g, fg_srgb.b)
        }
    };
    Resolved {
        ansi: theme.map(|t| t.ansi),
        selection_bg: theme.map(|t| t.selection_bg),
        // 0.85 alpha — the boot-path constant (`main.rs` theme block).
        cursor_color: theme.map(|t| crate::color_to_f32_rgba(&t.cursor, 0.85)),
        bg,
        fg,
        font_size: config.font_size * config.accessibility.font_scale,
        cursor_style: config.cursor.style,
        cursor_blink: config.cursor.blink && !config.accessibility.reduce_motion,
        cursor_blink_rate_ms: config.cursor.blink_rate_ms,
        padding: config.window.padding as f32,
        bold_is_bright: config.appearance.bold_is_bright,
        reduce_motion: config.accessibility.reduce_motion,
        effects: config.resolved_effects(),
    }
}

/// Bit-exact f32 change detection — "did the operator's edit change
/// this value at all". Epsilon comparison would mask small edits;
/// `==` would be a float-cmp footgun in review.
fn f32_changed(a: f32, b: f32) -> bool {
    a.to_bits() != b.to_bits()
}

fn rgba_changed(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(b.iter()).any(|(x, y)| f32_changed(*x, *y))
}

fn wgpu_color_changed(a: wgpu::Color, b: wgpu::Color) -> bool {
    a.r.to_bits() != b.r.to_bits()
        || a.g.to_bits() != b.g.to_bits()
        || a.b.to_bits() != b.b.to_bits()
        || a.a.to_bits() != b.a.to_bits()
}

/// PURE diff: resolve both configs to render values and emit one
/// [`SetterCall`] per changed value, in a fixed order (theme colors,
/// font, cursor, padding, bold-is-bright, reduce-motion, effects —
/// reduce-motion strictly before effects, mirroring
/// `apply_effects_and_accessibility`). Deterministic by
/// construction: no I/O, no clocks, no renderer reads.
#[must_use]
pub fn diff(old: &MadoConfig, new: &MadoConfig) -> Vec<SetterCall> {
    let o = resolve(old);
    let n = resolve(new);
    let mut calls = Vec::new();
    if let Some(ansi) = n.ansi {
        if o.ansi != Some(ansi) {
            calls.push(SetterCall::AnsiColors(ansi));
        }
    }
    if let Some(sel) = n.selection_bg {
        if o.selection_bg.is_none_or(|prev| rgba_changed(prev, sel)) {
            calls.push(SetterCall::SelectionBg(sel));
        }
    }
    if let Some(cur) = n.cursor_color {
        if o.cursor_color.is_none_or(|prev| rgba_changed(prev, cur)) {
            calls.push(SetterCall::CursorColor(cur));
        }
    }
    if wgpu_color_changed(o.bg, n.bg) || o.fg != n.fg {
        calls.push(SetterCall::BgFg { bg: n.bg, fg: n.fg });
    }
    if f32_changed(o.font_size, n.font_size) {
        calls.push(SetterCall::FontSize(n.font_size));
    }
    if o.cursor_style != n.cursor_style {
        calls.push(SetterCall::CursorStyle(n.cursor_style));
    }
    if o.cursor_blink != n.cursor_blink {
        calls.push(SetterCall::CursorBlink(n.cursor_blink));
    }
    if o.cursor_blink_rate_ms != n.cursor_blink_rate_ms {
        calls.push(SetterCall::CursorBlinkRateMs(n.cursor_blink_rate_ms));
    }
    if f32_changed(o.padding, n.padding) {
        calls.push(SetterCall::Padding(n.padding));
    }
    if o.bold_is_bright != n.bold_is_bright {
        calls.push(SetterCall::BoldIsBright(n.bold_is_bright));
    }
    if o.reduce_motion != n.reduce_motion {
        calls.push(SetterCall::ReduceMotion(n.reduce_motion));
    }
    if o.effects != n.effects {
        calls.push(SetterCall::Effects(n.effects));
    }
    calls
}

/// The executor half of the FSM step: walk the typed list, call the
/// matching setter. No logic here beyond dispatch — every decision
/// already happened in [`diff`].
pub fn execute<T: ConfigSetters>(target: &mut T, calls: Vec<SetterCall>) {
    for call in calls {
        match call {
            SetterCall::AnsiColors(v) => target.set_ansi_colors(v),
            SetterCall::SelectionBg(v) => target.set_selection_bg(v),
            SetterCall::CursorColor(v) => target.set_cursor_color(v),
            SetterCall::BgFg { bg, fg } => target.set_bg_fg(bg, fg),
            SetterCall::FontSize(v) => target.set_font_size(v),
            SetterCall::CursorStyle(v) => target.set_cursor_style(v),
            SetterCall::CursorBlink(v) => target.set_cursor_blink(v),
            SetterCall::CursorBlinkRateMs(v) => target.set_cursor_blink_rate_ms(v),
            SetterCall::Padding(v) => target.set_padding(v),
            SetterCall::BoldIsBright(v) => target.set_bold_is_bright(v),
            SetterCall::ReduceMotion(v) => target.set_reduce_motion(v),
            SetterCall::Effects(v) => target.set_effects_config(v),
        }
    }
}

/// Stateful wrapper: remembers the last-applied config, diffs each
/// incoming one against it, executes only the changed setters, then
/// advances `last`. Seed with the BOOT config (post-profile) so the
/// first reload diffs against what the renderer actually shows.
pub struct ConfigApplier {
    last: MadoConfig,
}

impl ConfigApplier {
    #[must_use]
    pub fn new(boot: MadoConfig) -> Self {
        Self { last: boot }
    }

    /// Diff `new` against the last-applied config and run only the
    /// changed setters. Returns the number of setter calls made —
    /// `0` whenever `new` resolves identically to the previous
    /// config.
    pub fn apply_delta<T: ConfigSetters>(&mut self, new: &MadoConfig, target: &mut T) -> usize {
        let calls = diff(&self.last, new);
        let n = calls.len();
        execute(target, calls);
        self.last = new.clone();
        n
    }
}

/// Clone-able handle pair shared between the shikumi watch callback
/// (writer: flips `dirty`) and the render loops (reader: polls).
/// The store is the ONE config source — the callback never carries
/// the config value itself.
#[derive(Clone)]
pub struct ConfigReloadSource {
    store: Arc<shikumi::ConfigStore<MadoConfig>>,
    dirty: Arc<AtomicBool>,
}

impl ConfigReloadSource {
    #[must_use]
    pub fn new(store: Arc<shikumi::ConfigStore<MadoConfig>>, dirty: Arc<AtomicBool>) -> Self {
        Self { store, dirty }
    }

    /// Consume the dirty edge: when the watcher flagged a reload,
    /// read the freshest config from the store (re-applying the
    /// active profile exactly like boot does) and clear the flag.
    /// `None` on the steady-state frame — the per-frame cost is one
    /// relaxed-ish atomic swap.
    #[must_use]
    pub fn take_if_dirty(&self) -> Option<MadoConfig> {
        if self.dirty.swap(false, Ordering::AcqRel) {
            Some(MadoConfig::clone(&self.store.get()).with_active_profile())
        } else {
            None
        }
    }
}

/// Per-render-loop hot-reload driver: ONE of these lives in each
/// event-loop closure; `poll_config_reload` runs once per frame.
/// Construct with the loop's boot config so the first delta is
/// computed against the state the renderer was actually built from.
pub struct ConfigHotReload {
    source: ConfigReloadSource,
    applier: ConfigApplier,
}

impl ConfigHotReload {
    #[must_use]
    pub fn new(source: ConfigReloadSource, boot: MadoConfig) -> Self {
        Self {
            source,
            applier: ConfigApplier::new(boot),
        }
    }

    /// Frame-start poll: cheap atomic check; on a flagged reload,
    /// delta-apply the freshest store config against the target.
    pub fn poll_config_reload<T: ConfigSetters>(&mut self, target: &mut T) {
        if let Some(new) = self.source.take_if_dirty() {
            let applied = self.applier.apply_delta(&new, target);
            tracing::info!(setter_calls = applied, "config reload delta applied");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counting double — records the NAME of every setter hit so
    /// tests can assert exact call sets (and, critically, exact
    /// absence: `apply_delta(x, x)` must record nothing).
    #[derive(Default)]
    struct CountingSetters {
        calls: Vec<&'static str>,
    }

    impl ConfigSetters for CountingSetters {
        fn set_ansi_colors(&mut self, _: [Color; 16]) { self.calls.push("set_ansi_colors"); }
        fn set_selection_bg(&mut self, _: [f32; 4]) { self.calls.push("set_selection_bg"); }
        fn set_cursor_color(&mut self, _: [f32; 4]) { self.calls.push("set_cursor_color"); }
        fn set_bg_fg(&mut self, _: wgpu::Color, _: Color) { self.calls.push("set_bg_fg"); }
        fn set_font_size(&mut self, _: f32) { self.calls.push("set_font_size"); }
        fn set_cursor_style(&mut self, _: CursorStyle) { self.calls.push("set_cursor_style"); }
        fn set_cursor_blink(&mut self, _: bool) { self.calls.push("set_cursor_blink"); }
        fn set_cursor_blink_rate_ms(&mut self, _: u32) { self.calls.push("set_cursor_blink_rate_ms"); }
        fn set_padding(&mut self, _: f32) { self.calls.push("set_padding"); }
        fn set_bold_is_bright(&mut self, _: bool) { self.calls.push("set_bold_is_bright"); }
        fn set_reduce_motion(&mut self, _: bool) { self.calls.push("set_reduce_motion"); }
        fn set_effects_config(&mut self, _: MadoEffectsConfig) { self.calls.push("set_effects_config"); }
    }

    fn call_kind(c: &SetterCall) -> &'static str {
        match c {
            SetterCall::AnsiColors(_) => "AnsiColors",
            SetterCall::SelectionBg(_) => "SelectionBg",
            SetterCall::CursorColor(_) => "CursorColor",
            SetterCall::BgFg { .. } => "BgFg",
            SetterCall::FontSize(_) => "FontSize",
            SetterCall::CursorStyle(_) => "CursorStyle",
            SetterCall::CursorBlink(_) => "CursorBlink",
            SetterCall::CursorBlinkRateMs(_) => "CursorBlinkRateMs",
            SetterCall::Padding(_) => "Padding",
            SetterCall::BoldIsBright(_) => "BoldIsBright",
            SetterCall::ReduceMotion(_) => "ReduceMotion",
            SetterCall::Effects(_) => "Effects",
        }
    }

    #[test]
    fn identical_configs_yield_zero_setter_calls() {
        // Matrix over distinct config shapes — every row must diff
        // to empty against itself AND drive zero setters through the
        // counting double.
        let mut themed = MadoConfig::default();
        themed.theme = "dracula".into();
        themed.font_size = 17.5;
        themed.effects.crt.enabled = true;
        themed.accessibility.reduce_motion = true;
        let mut unknown_theme = MadoConfig::default();
        unknown_theme.theme = "no-such-theme".into();
        let rows: Vec<(&str, MadoConfig)> = vec![
            ("default", MadoConfig::default()),
            ("bare", MadoConfig::bare()),
            ("themed+effects", themed),
            ("unknown-theme", unknown_theme),
        ];
        let mut failures = Vec::new();
        for (name, cfg) in rows {
            if !diff(&cfg, &cfg).is_empty() {
                failures.push(format!("{name}: diff(x, x) non-empty"));
            }
            let mut applier = ConfigApplier::new(cfg.clone());
            let mut counter = CountingSetters::default();
            let n = applier.apply_delta(&cfg, &mut counter);
            if n != 0 || !counter.calls.is_empty() {
                failures.push(format!(
                    "{name}: apply_delta(x, x) called {:?}",
                    counter.calls
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} identity rows failed:\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }

    #[test]
    fn theme_and_font_change_yields_exactly_those_setters() {
        let mut old = MadoConfig::default();
        old.theme = "nord".into();
        old.font_size = 14.0;
        let mut new = old.clone();
        new.theme = "dracula".into();
        new.font_size = 16.0;

        let calls = diff(&old, &new);
        let kinds: Vec<&str> = calls.iter().map(call_kind).collect();
        assert_eq!(
            kinds,
            vec!["AnsiColors", "SelectionBg", "CursorColor", "BgFg", "FontSize"],
            "theme+font edit must emit exactly the theme color group + font size, in fixed order; got {calls:?}"
        );
        let dracula = Theme::by_name("dracula").expect("dracula preset");
        assert!(calls.contains(&SetterCall::AnsiColors(dracula.ansi)));
        assert!(calls.contains(&SetterCall::FontSize(16.0)));
        // And through the executor: exactly those five setters fire.
        let mut applier = ConfigApplier::new(old);
        let mut counter = CountingSetters::default();
        let n = applier.apply_delta(&new, &mut counter);
        assert_eq!(n, 5);
        assert_eq!(
            counter.calls,
            vec![
                "set_ansi_colors",
                "set_selection_bg",
                "set_cursor_color",
                "set_bg_fg",
                "set_font_size"
            ]
        );
        // Applier advanced `last`: re-applying the same config is
        // now a zero-call no-op.
        let mut counter2 = CountingSetters::default();
        assert_eq!(applier.apply_delta(&new, &mut counter2), 0);
        assert!(counter2.calls.is_empty());
    }

    #[test]
    fn diff_is_deterministic_for_the_same_input_pair() {
        let mut old = MadoConfig::default();
        old.theme = "nord".into();
        let mut new = old.clone();
        new.theme = "one-dark".into();
        new.font_size = 18.0;
        new.cursor.style = CursorStyle::Bar;
        new.accessibility.reduce_motion = true;
        new.effects.bloom.enabled = true;
        // FSM step contract: same (state, event) → same effects list.
        assert_eq!(diff(&old, &new), diff(&old, &new));
    }

    #[test]
    fn reduce_motion_is_ordered_before_effects() {
        let old = MadoConfig::default();
        let mut new = old.clone();
        new.accessibility.reduce_motion = true;
        new.effects.snow.enabled = true;
        let kinds: Vec<&str> = diff(&old, &new).iter().map(call_kind).collect();
        let rm = kinds.iter().position(|k| *k == "ReduceMotion");
        let fx = kinds.iter().position(|k| *k == "Effects");
        // reduce_motion also gates cursor blink (default blink=true),
        // so CursorBlink appears too — the load-bearing assertion is
        // the relative order of the motion gate vs the effect set,
        // mirroring apply_effects_and_accessibility.
        assert!(
            rm < fx,
            "ReduceMotion must execute before Effects (got {kinds:?})"
        );
    }

    #[test]
    fn unknown_new_theme_emits_no_palette_calls() {
        // Boot ignores an unresolvable theme name for the palette /
        // selection / cursor colors (renderer keeps its current
        // ones); the diff mirrors that — no AnsiColors /
        // SelectionBg / CursorColor for a theme that resolves to no
        // preset. (BgFg MAY legitimately fire: with no theme, bg/fg
        // fall back to the appearance hexes, exactly like boot.)
        let mut old = MadoConfig::default();
        old.theme = "nord".into();
        let mut new = old.clone();
        new.theme = "definitely-not-a-theme".into();
        let kinds: Vec<&str> = diff(&old, &new).iter().map(call_kind).collect();
        for banned in ["AnsiColors", "SelectionBg", "CursorColor"] {
            assert!(
                !kinds.contains(&banned),
                "unknown theme must not emit {banned}, got {kinds:?}"
            );
        }
    }

    #[test]
    fn cursor_padding_and_flags_diff_per_field() {
        let old = MadoConfig::default();
        let mut new = old.clone();
        new.cursor.style = CursorStyle::Underline;
        new.cursor.blink_rate_ms = 250;
        new.window.padding = 12;
        new.appearance.bold_is_bright = true;
        let kinds: Vec<&str> = diff(&old, &new).iter().map(call_kind).collect();
        assert_eq!(
            kinds,
            vec!["CursorStyle", "CursorBlinkRateMs", "Padding", "BoldIsBright"]
        );
    }

    #[test]
    fn reload_source_take_if_dirty_is_a_consumed_edge() {
        let dir = std::env::temp_dir().join("mado-config-apply-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("mado.yaml");
        std::fs::write(&path, "font_size: 19.0\n").expect("write config");
        let store = shikumi::ConfigStore::<MadoConfig>::load(&path, "MADO_TEST_NOPREFIX_")
            .expect("store load");
        let dirty = Arc::new(AtomicBool::new(false));
        let source = ConfigReloadSource::new(Arc::new(store), Arc::clone(&dirty));
        assert!(source.take_if_dirty().is_none(), "clean flag → no config");
        dirty.store(true, Ordering::Release);
        let cfg = source.take_if_dirty().expect("dirty flag → store config");
        assert!((cfg.font_size - 19.0).abs() < f32::EPSILON);
        assert!(!dirty.load(Ordering::Acquire), "edge must clear the flag");
        assert!(source.take_if_dirty().is_none(), "second take is empty");
    }
}
