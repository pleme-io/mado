//! Platform-specific integration.
//!
//! macOS: window styling via objc2 safe bindings (transparent titlebar, native appearance).
//! Linux: placeholder for Wayland-specific integration.

/// Operator-configured macOS window-chrome inputs, extracted from
/// `MadoConfig` before the event loop so the (`'static`) loop closure
/// owns a small value instead of borrowing the whole config. Fields
/// mirror `config.window.macos` plus the resolved backing color from
/// `config.appearance.background`. Every axis here is a shikumi config
/// value — the operator controls all of it via `~/.config/mado/mado.yaml`.
#[derive(Debug, Clone, PartialEq)]
pub struct MacOsWindowStyle {
    /// Allow the macOS-native window tab bar (`window.macos.native_tabs`).
    pub native_tabs: bool,
    /// Titlebar integration style (`window.macos.titlebar`).
    pub titlebar: crate::config::TitlebarStyle,
    /// Forced window appearance (`window.macos.appearance`).
    pub appearance: crate::config::WindowAppearance,
    /// sRGB window backing color, resolved from
    /// `config.appearance.background`. A `Flush` titlebar tints the
    /// NSWindow backing to this so the band matches the cell grid.
    pub background: ishou_tokens::Srgb,
}

impl MacOsWindowStyle {
    /// Extract the chrome inputs from the operator's shikumi config.
    /// Both event-loop paths (local-PTY `main.rs`, tear-attach
    /// `gui_tear_attach.rs`) call this so the chrome contract stays
    /// one shape.
    pub fn from_config(config: &crate::config::MadoConfig) -> Self {
        // The band tint resolves the same way the renderer's clear
        // color does: the named theme's background wins,
        // `appearance.background` is the fallback. Under a Flush band
        // the NSWindow backing is the only paint behind the titlebar,
        // so any divergence from the grid's clear color reads as an
        // off-colour strip.
        let fallback = ishou_tokens::Srgb::from_hex(&config.appearance.background)
            .unwrap_or(ishou_tokens::Srgb::new(0x2e, 0x34, 0x40));
        let background = crate::theme::Theme::by_name(&config.theme)
            .map(|t| ishou_tokens::Srgb::new(t.background.r, t.background.g, t.background.b))
            .unwrap_or(fallback);
        Self {
            native_tabs: config.window.macos.native_tabs,
            titlebar: config.window.macos.titlebar,
            appearance: config.window.macos.appearance,
            background,
        }
    }
}

/// One-shot latch tying the window-chrome style to its applied state.
/// Event loops construct one before entering the loop and call
/// [`tick`](Self::tick) on every event — styling applies on the first
/// tick where a window actually exists, then the latch goes inert.
///
/// This is the only sanctioned consumption shape for
/// [`apply_native_styling`]: holding the flag and the style in one
/// value makes the historical fire-once bug (flag set before the
/// styling actually landed, leaving the stock grey titlebar up
/// forever) unwritable at the call site.
pub struct NativeStylingLatch {
    style: MacOsWindowStyle,
    applied: bool,
}

impl NativeStylingLatch {
    /// Build the latch from the operator's shikumi config.
    pub fn from_config(config: &crate::config::MadoConfig) -> Self {
        Self {
            style: MacOsWindowStyle::from_config(config),
            applied: false,
        }
    }

    /// Apply styling if it hasn't landed yet; cheap no-op afterwards.
    pub fn tick(&mut self) {
        if !self.applied {
            self.applied = apply_native_styling(&self.style);
        }
    }

    /// Re-derive the chrome style from a reloaded config and, if it
    /// actually changed, un-latch so the next [`tick`](Self::tick)
    /// re-applies it to every live window.
    ///
    /// Boot-time resolution only ever runs once (see [`tick`]'s own
    /// doc), so a runtime theme switch (hot-reload edit, or the
    /// `set_theme` MCP tool) previously left the NSWindow/titlebar
    /// backing pinned to whatever `MacOsWindowStyle::from_config`
    /// resolved at launch while the cell-grid clear color moved live
    /// — the reported bug: a titlebar strip stuck on the bare-tier
    /// `#000000` fallback (or whatever theme was active at boot)
    /// while the canvas below correctly showed the new theme. This
    /// closes that gap by giving hot-reload the same "state changed →
    /// re-apply" contract boot already had.
    pub fn refresh(&mut self, config: &crate::config::MadoConfig) {
        let new_style = MacOsWindowStyle::from_config(config);
        if new_style != self.style {
            self.style = new_style;
            self.applied = false;
        }
    }
}

/// Apply platform-native window styling from the operator config.
/// On macOS this drives titlebar integration, native-tab suppression,
/// and forced appearance — all shikumi-configured; a no-op elsewhere.
///
/// Returns `true` once styling has actually been applied to at least
/// one window (or there is nothing to do on this platform). Returns
/// `false` when it could not apply: no window exists yet — during
/// launch the first event ticks can arrive before AppKit registers
/// the window — or the call is off the main thread. Callers MUST
/// retry on subsequent main-thread ticks until this reports `true`
/// (a fire-once call here is exactly the bug that left the stock
/// grey titlebar + visible `❄` title on tear-attach windows); prefer
/// [`NativeStylingLatch`], which packages that contract.
#[must_use]
pub fn apply_native_styling(style: &MacOsWindowStyle) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::apply_styling(style)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = style;
        tracing::debug!("no platform-specific styling for this OS");
        true
    }
}

/// Set the macOS dock icon badge text (e.g., for bell notifications).
#[allow(dead_code)]
pub fn set_badge(_text: Option<&str>) {
    #[cfg(target_os = "macos")]
    macos::set_badge(_text);
}

/// Bounce the dock icon / flash the taskbar — the platform half of
/// OSC 1337 `RequestAttention`. Critical request: the bounce repeats
/// until the operator focuses the window (macOS semantics). Safe
/// no-op off the main thread and on non-macOS platforms.
pub fn request_dock_attention() {
    #[cfg(target_os = "macos")]
    macos::request_dock_attention();

    #[cfg(not(target_os = "macos"))]
    tracing::debug!("dock attention requested — no platform surface on this OS");
}

/// Ring the system audible bell — `None` plays the classic system beep
/// (`NSBeep`); `Some(name)` plays a named `NSSound` (e.g. `"Basso"`,
/// `"Ping"`), falling back to `NSBeep` when the name is unknown. Playback
/// is async and non-blocking. Safe no-op off macOS. This is the real
/// audible bell — previously the "audible bell" was fictional (only the
/// visual flash + glow fired).
pub fn ring_bell(sound: Option<&str>) {
    #[cfg(target_os = "macos")]
    macos::ring_bell(sound);

    #[cfg(not(target_os = "macos"))]
    let _ = sound;
}

/// Construct the boot-time notification dispatcher for the GUI event
/// loops. Defaults ([`NotifyBackend::Auto`](crate::config::NotifyBackend))
/// to the native `UNUserNotificationCenter` backend when mado runs
/// bundled (`Mado.app`) — mado-attributed banners with sound + urgency,
/// **no Script-Editor popup** — and to a silent `LogBackend` otherwise.
/// `osascript` is opt-in only. Headless/test environments construct their
/// own `LogBackend` dispatcher directly.
#[must_use]
pub fn notification_dispatcher(
    choice: crate::config::NotifyBackend,
) -> tsuuchi::NotificationDispatcher {
    use crate::config::NotifyBackend;
    #[cfg(target_os = "macos")]
    {
        use tsuuchi::NotificationDispatcher as D;
        match choice {
            NotifyBackend::Osascript => {
                tracing::info!(
                    "notifications: osascript backend (opt-in; Script-Editor-attributed)"
                );
                D::new(Box::new(macos::OsaScriptBackend))
            }
            NotifyBackend::Log => D::new(Box::new(tsuuchi::LogBackend::new())),
            NotifyBackend::Auto | NotifyBackend::Native => {
                if let Some(un) = tsuuchi::UnBackend::try_new() {
                    tracing::info!("notifications: native UNUserNotificationCenter backend");
                    D::new(Box::new(un))
                } else {
                    tracing::info!(
                        "notifications: unbundled — LogBackend (no banner, no popup); \
                         run Mado.app for native notifications"
                    );
                    D::new(Box::new(tsuuchi::LogBackend::new()))
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = choice;
        tsuuchi::NotificationDispatcher::new(Box::new(tsuuchi::LogBackend::new()))
    }
}

/// Install mado's own macOS menubar in place of the one winit builds.
///
/// **Why mado owns this at all.** winit installs a default menubar on
/// macOS — About / Services / Hide / Hide Others / Show All / Quit
/// (`winit-0.30.13/src/platform_impl/macos/menu.rs`) — and it is
/// all-or-nothing: winit exposes no API to drop a single item. So the only
/// way to not ship a Services submenu is to decline the whole default
/// (`madori::MenuPolicy::AppOwned`) and build the menu here.
///
/// **Why Services is the item that had to go.** winit calls
/// `app.setServicesMenu(...)`, which advertises mado as a Services
/// participant. A Service acts on the app's *selection*, which it obtains
/// through `NSServicesMenuRequestor` — `validRequestorForSendType:
/// returnType:` plus `writeSelectionToPasteboard:types:`. mado implements
/// none of them, so no service could ever read mado's terminal selection:
/// the submenu could only ever list system-wide entries that ignore the
/// app. It was a menu that structurally could not do the one thing it
/// looks like it does. (If mado ever wants real text services — "Search
/// With…", "New Note With Selection" — that is implementing the requestor
/// protocol over the existing selection buffer, a feature, not a menu
/// entry.)
///
/// **What is kept, and why it is not optional.** ⌘Q and ⌘H are *menu key
/// equivalents* on macOS — they are `terminate:` and `hide:` hung off menu
/// items, not application-level shortcuts. Declining winit's menu without
/// replacing it would ship a terminal the operator cannot quit with ⌘Q and
/// an empty Apple-menu-only menubar, which reads as broken rather than
/// minimal. So About / Hide / Quit stay; Services, Hide Others and Show
/// All go.
pub fn install_app_menu() {
    #[cfg(target_os = "macos")]
    {
        macos::install_app_menu();
    }
}

/// Check if the system is in dark mode.
#[must_use]
pub fn is_dark_mode() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::is_dark_mode()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true // Default to dark mode on unknown platforms
    }
}

// ── Follow-OS light/dark ─────────────────────────────────────────────
//
// [`is_dark_mode`] shipped long ago and had exactly ONE call site: a
// boot-time `tracing::debug!`. It was never *used* — flipping the macOS
// appearance while mado ran changed nothing. This is the live half.
//
// **One setter path, not two.** The appearance edge does NOT reach the
// renderer directly. It picks the operator's `light` / `dark` PROFILE,
// folds it through `MadoConfig::with_profile`, and hands the resulting
// config to the SAME `ux::config_apply::ConfigApplier` the watched-file
// hot-reload uses (`ConfigHotReload::apply_external`). So an OS flip and
// a `mado.yaml` edit converge through one diff and one executor — there
// is no second theme-application path to drift.

/// Profile name mado activates for a given system appearance. The names
/// are the ones `ProfileConfig`'s own doc-comment example already uses
/// (`profiles: { light: { theme: "solarized_light", … } }`), so this is
/// the shipped, documented mechanism rather than a new config leaf.
#[must_use]
pub fn appearance_profile(dark: bool) -> &'static str {
    if dark { "dark" } else { "light" }
}

/// How often [`FollowOsAppearance::poll`] re-reads the system
/// appearance. The read is one `NSUserDefaults` string lookup; the
/// throttle keeps a 120 Hz redraw loop from performing it 120×/s while
/// still picking a flip up well inside human reaction time.
pub const APPEARANCE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Live system-appearance follower — the edge detector behind
/// follow-OS light/dark.
///
/// Constructed ONLY when the operator has actually asked for it:
/// `window.macos.appearance: auto` (the typed "follow the system"
/// declaration) AND at least one of the `light` / `dark` profiles is
/// defined. Without a profile to switch INTO there is nothing to apply,
/// and silently picking a theme for the operator would be a surprise,
/// not a feature — so that case stays dormant rather than guessing.
pub struct FollowOsAppearance {
    /// The appearance the applied config corresponds to, seeded at
    /// construction so boot never fires a spurious edge.
    last_dark: bool,
    /// Next instant [`poll`](Self::poll) is allowed to hit the OS.
    next_poll: std::time::Instant,
}

impl FollowOsAppearance {
    /// `None` when follow-OS is not requested (see the type docs).
    #[must_use]
    pub fn from_config(config: &crate::config::MadoConfig) -> Option<Self> {
        if config.window.macos.appearance != crate::config::WindowAppearance::Auto {
            return None;
        }
        let has_profile = config.profiles.contains_key(appearance_profile(true))
            || config.profiles.contains_key(appearance_profile(false));
        if !has_profile {
            tracing::debug!(
                "window.macos.appearance = auto but neither a `light` nor a `dark` profile is \
                 defined — follow-OS theme switching stays dormant"
            );
            return None;
        }
        Some(Self {
            last_dark: is_dark_mode(),
            next_poll: std::time::Instant::now() + APPEARANCE_POLL_INTERVAL,
        })
    }

    /// PURE edge detector: `Some(dark)` exactly on a change, `None`
    /// while the appearance is steady. Split out from [`poll`](Self::poll)
    /// so the edge semantics are testable without an OS.
    pub fn observe(&mut self, dark: bool) -> Option<bool> {
        if dark == self.last_dark {
            return None;
        }
        self.last_dark = dark;
        Some(dark)
    }

    /// Throttled OS read + edge. Call once per redraw; `Some(dark)` only
    /// on an actual flip.
    pub fn poll(&mut self, now: std::time::Instant) -> Option<bool> {
        if now < self.next_poll {
            return None;
        }
        self.next_poll = now + APPEARANCE_POLL_INTERVAL;
        self.observe(is_dark_mode())
    }
}

// ── Quick Terminal ───────────────────────────────────────────────────
//
// The typed `QuickTerminalConfig` (edge / size_fraction / animation_ms /
// autohide_on_blur / hotkey) plus its `resolve_size_pixels` +
// `resolve_origin_pixels` math shipped in config.rs with ZERO consumers.
// This is the consumer.

/// A screen rectangle in Cocoa's BOTTOM-left-origin space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Convert the TOP-left-origin placement `QuickTerminalConfig` resolves
/// into the BOTTOM-left-origin frame `NSWindow::setFrame_*` wants,
/// inside the screen's `visible` frame (which already excludes the menu
/// bar and the Dock — the right box for a drop-down).
///
/// This flip is the whole reason the geometry lives here rather than
/// inline at the AppKit call: `QuickTerminalEdge::Top` resolves to
/// `origin = (0, 0)`, which in Cocoa means the BOTTOM of the screen. A
/// quick terminal configured to drop from the top that instead rises
/// from the bottom is the bug this function exists to make testable.
#[must_use]
pub fn cocoa_frame(visible: ScreenRect, size: (u32, u32), top_left: (u32, u32)) -> ScreenRect {
    let w = f64::from(size.0);
    let h = f64::from(size.1);
    ScreenRect {
        x: visible.x + f64::from(top_left.0),
        y: visible.y + visible.h - f64::from(top_left.1) - h,
        w,
        h,
    }
}

/// What a Quick Terminal toggle actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickTerminalToggle {
    /// Placed at the resolved edge and ordered front.
    Shown,
    /// Ordered out.
    Hidden,
    /// Hide REFUSED. mado has no system-wide hotkey registration (see
    /// [`GlobalHotkeyStatus::AppScopedOnly`]), so an app-scoped chord
    /// cannot reach an ordered-out window: hiding the only window would
    /// strand the operator with a running, invisible, unreachable
    /// terminal. Show-only until a restore path exists.
    HideRefusedNoRestorePath,
}

/// Whether the operator's `quick_terminal.hotkey` is registered
/// system-wide, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalHotkeyStatus {
    /// No hotkey asked for — `quick_terminal.hotkey` is empty
    /// (`QuickTerminalConfig::is_active_hotkey()` is false).
    NotRequested,
    /// A chord was configured and parsed, and it toggles the Quick
    /// Terminal whenever mado holds focus — but there is NO system-wide
    /// registration, so it does nothing while another app is frontmost.
    ///
    /// Registering one genuinely needs an API mado does not have today:
    ///
    /// * Carbon `RegisterEventHotKey` — the real thing (exclusive,
    ///   consumes the event, no permission prompt). It is raw C FFI:
    ///   there is no objc2 binding, so it means an `extern "C"` block
    ///   and `unsafe` calls in this file.
    /// * `NSEvent::addGlobalMonitorForEventsMatchingMask_handler` — SAFE
    ///   in objc2-app-kit 0.3 (it is a `pub fn`, not `pub unsafe fn`),
    ///   but it needs the `NSEvent` + `block2` features added to
    ///   `objc2-app-kit` and a `block2` dependency in `Cargo.toml`, it
    ///   requires the operator to grant Accessibility/Input-Monitoring
    ///   in System Settings (silently never fires otherwise), and it
    ///   cannot CONSUME the chord — the frontmost app still receives it.
    ///
    /// Both are decisions about mado's dependency + permission surface,
    /// not implementation details, so the wiring stops here and reports.
    AppScopedOnly,
    /// A chord was configured but `awase` could not parse it.
    Unparseable,
}

/// The Quick Terminal: a drop-down window pinned to a screen edge,
/// toggled by a chord.
///
/// Every knob under `quick_terminal.*` reaches runtime behaviour here:
/// `enabled` gates construction, `edge` + `size_fraction` drive the
/// placement through the config's own resolvers, `animation_ms` selects
/// animated-vs-snap, `autohide_on_blur` drives [`on_focus_lost`], and
/// `hotkey` is parsed by `awase` into the chord [`matches`] compares.
///
/// [`on_focus_lost`]: Self::on_focus_lost
/// [`matches`]: Self::matches
pub struct QuickTerminal {
    cfg: crate::config::QuickTerminalConfig,
    hotkey: Option<awase::Hotkey>,
    /// Set when the configured hotkey string failed to parse — kept so
    /// [`hotkey_status`](Self::hotkey_status) can say so instead of
    /// looking like "no hotkey configured".
    hotkey_unparseable: bool,
    visible: bool,
    /// True once a restore path exists (a registered system-wide hotkey
    /// or another out-of-app trigger). While false, hiding is refused —
    /// see [`QuickTerminalToggle::HideRefusedNoRestorePath`]. This is
    /// the ONE flag a future global-hotkey registration flips; nothing
    /// else about the toggle changes.
    can_restore: bool,
    /// Placement has actually landed on a live NSWindow. The first
    /// redraws can arrive before AppKit registers the window, so
    /// [`tick`](Self::tick) retries — same contract as
    /// [`NativeStylingLatch`].
    placed: bool,
}

impl QuickTerminal {
    /// `None` when `quick_terminal.enabled` is false — the whole
    /// machinery stays dormant, as the config's own docs promise.
    #[must_use]
    pub fn from_config(config: &crate::config::MadoConfig) -> Option<Self> {
        let cfg = config.quick_terminal.clone();
        if !cfg.enabled {
            return None;
        }
        let (hotkey, hotkey_unparseable) = if cfg.hotkey.is_empty() {
            (None, false)
        } else {
            match awase::Hotkey::parse(&cfg.hotkey) {
                Ok(hk) => (Some(hk), false),
                Err(e) => {
                    tracing::warn!(
                        hotkey = %cfg.hotkey,
                        err = %e,
                        "quick_terminal.hotkey did not parse — no toggle chord bound"
                    );
                    (None, true)
                }
            }
        };
        Some(Self {
            cfg,
            hotkey,
            hotkey_unparseable,
            // A Quick Terminal starts VISIBLE: mado's window is already
            // on screen when the process launches, and ordering it out
            // at boot with no restore path is exactly the strand this
            // type refuses elsewhere.
            visible: true,
            can_restore: false,
            placed: false,
        })
    }

    /// Registration state of the configured chord. Reported once at
    /// boot so "my quick-terminal hotkey does nothing from another app"
    /// is answerable from the log instead of the source.
    #[must_use]
    pub fn hotkey_status(&self) -> GlobalHotkeyStatus {
        if self.hotkey_unparseable {
            return GlobalHotkeyStatus::Unparseable;
        }
        if self.hotkey.is_some() {
            GlobalHotkeyStatus::AppScopedOnly
        } else {
            GlobalHotkeyStatus::NotRequested
        }
    }

    /// Does this chord toggle the Quick Terminal?
    #[must_use]
    pub fn matches(&self, hotkey: &awase::Hotkey) -> bool {
        self.hotkey.as_ref() == Some(hotkey)
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// PURE: what a toggle WOULD do, with no AppKit involved.
    #[must_use]
    pub fn plan_toggle(&self) -> QuickTerminalToggle {
        if self.visible {
            if self.can_restore {
                QuickTerminalToggle::Hidden
            } else {
                QuickTerminalToggle::HideRefusedNoRestorePath
            }
        } else {
            QuickTerminalToggle::Shown
        }
    }

    /// Toggle visibility, applying the placement on the way in.
    pub fn toggle(&mut self) -> QuickTerminalToggle {
        let plan = self.plan_toggle();
        match plan {
            QuickTerminalToggle::Shown => {
                self.visible = true;
                self.apply();
            }
            QuickTerminalToggle::Hidden => {
                self.visible = false;
                self.apply();
            }
            QuickTerminalToggle::HideRefusedNoRestorePath => {
                tracing::info!(
                    "quick terminal: hide refused — no system-wide hotkey is registered, so an \
                     ordered-out window could not be brought back"
                );
            }
        }
        plan
    }

    /// `quick_terminal.autohide_on_blur`: drop out of sight when the
    /// window loses focus. Subject to the same restore-path refusal —
    /// a blur-hide with no way back would strand the operator on the
    /// first Cmd-Tab, which is worse than not autohiding.
    pub fn on_focus_lost(&mut self) -> Option<QuickTerminalToggle> {
        if !self.cfg.autohide_on_blur || !self.visible {
            return None;
        }
        Some(self.toggle())
    }

    /// Retry placement until a window exists; inert afterwards.
    pub fn tick(&mut self) {
        if !self.placed {
            self.apply();
        }
    }

    fn apply(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.placed = macos::quick_terminal_apply(&self.cfg, self.visible);
        }
        #[cfg(not(target_os = "macos"))]
        {
            tracing::debug!("quick terminal has no window surface on this OS");
            self.placed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_dark_mode_returns_bool() {
        let _ = is_dark_mode();
    }

    #[test]
    fn set_badge_none_does_not_panic() {
        set_badge(None);
    }

    #[test]
    fn set_badge_some_does_not_panic() {
        set_badge(Some("test"));
    }

    #[test]
    fn refresh_is_a_noop_when_the_resolved_style_is_unchanged() {
        let config = crate::config::MadoConfig::default();
        let mut latch = NativeStylingLatch::from_config(&config);
        latch.applied = true; // simulate boot-time styling having already landed
        latch.refresh(&config);
        assert!(latch.applied, "unchanged config must not un-latch");
    }

    #[test]
    fn refresh_unlatches_when_the_resolved_style_changes() {
        let mut config = crate::config::MadoConfig::default();
        let mut latch = NativeStylingLatch::from_config(&config);
        latch.applied = true; // simulate boot-time styling having already landed
        assert_ne!(
            config.window.macos.titlebar,
            crate::config::TitlebarStyle::Native,
            "test needs a change from the actual default, not a no-op"
        );
        config.window.macos.titlebar = crate::config::TitlebarStyle::Native;
        latch.refresh(&config);
        assert!(
            !latch.applied,
            "a chrome-relevant config change must un-latch so tick() re-applies"
        );
        assert_eq!(latch.style, MacOsWindowStyle::from_config(&config));
    }

    #[test]
    fn apply_native_styling_off_main_thread_is_a_safe_noop() {
        let style = MacOsWindowStyle {
            native_tabs: false,
            titlebar: crate::config::TitlebarStyle::default(),
            appearance: crate::config::WindowAppearance::default(),
            background: ishou_tokens::Srgb::new(0x2e, 0x34, 0x40),
        };
        // Spawn so this is never the process main thread.
        let applied = std::thread::spawn(move || apply_native_styling(&style))
            .join()
            .unwrap();
        // On macOS the off-main-thread path must report not-applied so
        // the event loop retries; elsewhere it's a successful no-op.
        #[cfg(target_os = "macos")]
        assert!(!applied);
        #[cfg(not(target_os = "macos"))]
        assert!(applied);
    }

    // ── Quick Terminal ───────────────────────────────────────────

    /// The whole reason `cocoa_frame` exists: `QuickTerminalConfig`
    /// resolves TOP-left origins, AppKit wants BOTTOM-left. A top-edge
    /// drop-down must land at the TOP of the visible frame.
    #[test]
    fn cocoa_frame_flips_the_origin_so_top_means_top() {
        let visible = ScreenRect {
            x: 0.0,
            y: 25.0, // Dock reserves the bottom band
            w: 1600.0,
            h: 1000.0,
        };
        // Top edge, 40% → full width × 400px, top_left origin (0, 0).
        let top = cocoa_frame(visible, (1600, 400), (0, 0));
        assert_eq!(top.w, 1600.0);
        assert_eq!(top.h, 400.0);
        assert_eq!(top.x, 0.0);
        // Cocoa y of the pane's BOTTOM edge = visible top (25+1000) - 400.
        assert_eq!(top.y, 625.0);
        assert_eq!(
            top.y + top.h,
            visible.y + visible.h,
            "a top-edge quick terminal's upper edge must touch the top of the visible frame"
        );

        // Bottom edge: origin_top_left.y = screen_h - h = 600.
        let bottom = cocoa_frame(visible, (1600, 400), (0, 600));
        assert_eq!(
            bottom.y, visible.y,
            "a bottom-edge quick terminal sits ON the visible frame's floor"
        );
        assert_ne!(top.y, bottom.y, "top and bottom must not resolve alike");
    }

    /// The geometry the config resolves + the flip must agree for every
    /// edge: the pane always lies inside the visible frame, and each
    /// edge pins the side it names.
    #[test]
    fn every_quick_terminal_edge_lands_inside_the_visible_frame() {
        use crate::config::{QuickTerminalConfig, QuickTerminalEdge};
        let visible = ScreenRect {
            x: 10.0,
            y: 25.0,
            w: 1600.0,
            h: 1000.0,
        };
        let screen_px = (1600u32, 1000u32);
        let mut failures = Vec::new();
        for edge in [
            QuickTerminalEdge::Top,
            QuickTerminalEdge::Bottom,
            QuickTerminalEdge::Left,
            QuickTerminalEdge::Right,
            QuickTerminalEdge::Center,
        ] {
            let cfg = QuickTerminalConfig {
                enabled: true,
                edge,
                size_fraction: 0.4,
                ..QuickTerminalConfig::default()
            };
            let size = cfg.resolve_size_pixels(screen_px);
            let tl = cfg.resolve_origin_pixels(screen_px);
            let f = cocoa_frame(visible, size, tl);
            if f.x < visible.x || f.y < visible.y {
                failures.push(format!(
                    "{edge:?}: origin escapes the visible frame ({f:?})"
                ));
            }
            if f.x + f.w > visible.x + visible.w || f.y + f.h > visible.y + visible.h {
                failures.push(format!(
                    "{edge:?}: extent escapes the visible frame ({f:?})"
                ));
            }
            let pinned = match edge {
                QuickTerminalEdge::Top => f.y + f.h == visible.y + visible.h,
                QuickTerminalEdge::Bottom => f.y == visible.y,
                QuickTerminalEdge::Left => f.x == visible.x,
                QuickTerminalEdge::Right => f.x + f.w == visible.x + visible.w,
                QuickTerminalEdge::Center => true,
            };
            if !pinned {
                failures.push(format!("{edge:?}: does not pin the edge it names ({f:?})"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n  - "));
    }

    #[test]
    fn quick_terminal_is_dormant_unless_enabled() {
        let config = crate::config::MadoConfig::default();
        assert!(
            !config.quick_terminal.enabled,
            "test needs the shipped opt-in default"
        );
        assert!(QuickTerminal::from_config(&config).is_none());
    }

    #[test]
    fn quick_terminal_parses_the_configured_chord_and_reports_app_scope() {
        let mut config = crate::config::MadoConfig::default();
        config.quick_terminal.enabled = true;
        config.quick_terminal.hotkey = "cmd+grave".into();
        let qt = QuickTerminal::from_config(&config).expect("enabled → Some");
        assert_eq!(qt.hotkey_status(), GlobalHotkeyStatus::AppScopedOnly);
        let chord = awase::Hotkey::parse("cmd+grave").expect("chord parses");
        assert!(qt.matches(&chord));
        assert!(!qt.matches(&awase::Hotkey::parse("cmd+k").expect("other chord")));
    }

    #[test]
    fn quick_terminal_without_a_hotkey_is_mcp_only_not_broken() {
        let mut config = crate::config::MadoConfig::default();
        config.quick_terminal.enabled = true;
        // The config's own doc: enabled + empty hotkey is VALID.
        let qt = QuickTerminal::from_config(&config).expect("enabled → Some");
        assert_eq!(qt.hotkey_status(), GlobalHotkeyStatus::NotRequested);
        assert!(!config.quick_terminal.is_active_hotkey());
    }

    #[test]
    fn quick_terminal_reports_an_unparseable_chord_as_such() {
        let mut config = crate::config::MadoConfig::default();
        config.quick_terminal.enabled = true;
        config.quick_terminal.hotkey = "!!not a chord!!".into();
        let qt = QuickTerminal::from_config(&config).expect("enabled → Some");
        assert_eq!(
            qt.hotkey_status(),
            GlobalHotkeyStatus::Unparseable,
            "a typo'd chord must not look like `no hotkey configured`"
        );
    }

    /// The strand invariant: with no system-wide hotkey there is no way
    /// to bring an ordered-out window back, so hiding is REFUSED.
    #[test]
    fn quick_terminal_refuses_to_hide_with_no_restore_path() {
        let mut config = crate::config::MadoConfig::default();
        config.quick_terminal.enabled = true;
        config.quick_terminal.hotkey = "cmd+grave".into();
        let mut qt = QuickTerminal::from_config(&config).expect("enabled → Some");
        assert!(qt.is_visible(), "mado's window is on screen at launch");
        assert_eq!(
            qt.plan_toggle(),
            QuickTerminalToggle::HideRefusedNoRestorePath
        );
        assert_eq!(qt.toggle(), QuickTerminalToggle::HideRefusedNoRestorePath);
        assert!(
            qt.is_visible(),
            "a refused hide must leave the window visible, not half-toggle it"
        );

        // The ONE flag a future global-hotkey registration flips.
        qt.can_restore = true;
        assert_eq!(qt.plan_toggle(), QuickTerminalToggle::Hidden);
        assert_eq!(qt.toggle(), QuickTerminalToggle::Hidden);
        assert!(!qt.is_visible());
        assert_eq!(qt.plan_toggle(), QuickTerminalToggle::Shown);
    }

    #[test]
    fn autohide_on_blur_is_honoured_only_when_configured_and_restorable() {
        let mut config = crate::config::MadoConfig::default();
        config.quick_terminal.enabled = true;
        config.quick_terminal.autohide_on_blur = true;
        let mut qt = QuickTerminal::from_config(&config).expect("enabled → Some");
        // Restore path absent → the blur-hide is refused, not silent.
        assert_eq!(
            qt.on_focus_lost(),
            Some(QuickTerminalToggle::HideRefusedNoRestorePath)
        );
        assert!(qt.is_visible());
        qt.can_restore = true;
        assert_eq!(qt.on_focus_lost(), Some(QuickTerminalToggle::Hidden));
        assert!(!qt.is_visible());
        // Already hidden → nothing to do.
        assert_eq!(qt.on_focus_lost(), None);

        // Knob OFF → blur is ignored entirely.
        config.quick_terminal.autohide_on_blur = false;
        let mut qt = QuickTerminal::from_config(&config).expect("enabled → Some");
        qt.can_restore = true;
        assert_eq!(
            qt.on_focus_lost(),
            None,
            "autohide_on_blur: false must not hide on blur"
        );
        assert!(qt.is_visible());
    }

    // ── Follow-OS light/dark ─────────────────────────────────────

    #[test]
    fn follow_os_needs_both_auto_appearance_and_a_profile() {
        let mut config = crate::config::MadoConfig::default();
        // Default is a FORCED appearance, so follow-OS is off.
        assert_ne!(
            config.window.macos.appearance,
            crate::config::WindowAppearance::Auto
        );
        assert!(FollowOsAppearance::from_config(&config).is_none());

        // Auto alone is not enough — there is nothing to switch into.
        config.window.macos.appearance = crate::config::WindowAppearance::Auto;
        assert!(
            FollowOsAppearance::from_config(&config).is_none(),
            "auto with no light/dark profile must stay dormant, not guess a theme"
        );

        // Auto + a `light` profile → live.
        // Hyphen, not underscore: `solarized-light` is the registered
        // irodzuki preset name. (`ProfileConfig`'s doc-comment example
        // says `solarized_light`, which resolves to no theme at all.)
        config.profiles.insert(
            "light".into(),
            crate::config::ProfileConfig {
                theme: Some("solarized-light".into()),
                ..Default::default()
            },
        );
        assert!(FollowOsAppearance::from_config(&config).is_some());
    }

    #[test]
    fn follow_os_fires_only_on_an_edge() {
        let mut config = crate::config::MadoConfig::default();
        config.window.macos.appearance = crate::config::WindowAppearance::Auto;
        config
            .profiles
            .insert("dark".into(), crate::config::ProfileConfig::default());
        let mut f = FollowOsAppearance::from_config(&config).expect("auto + profile → Some");
        // Seed deterministically instead of depending on the host's
        // actual appearance.
        f.last_dark = true;
        assert_eq!(f.observe(true), None, "steady state emits nothing");
        assert_eq!(f.observe(false), Some(false), "dark→light is an edge");
        assert_eq!(f.observe(false), None, "the edge is consumed");
        assert_eq!(f.observe(true), Some(true), "light→dark is an edge");
    }

    #[test]
    fn follow_os_poll_is_throttled() {
        let mut config = crate::config::MadoConfig::default();
        config.window.macos.appearance = crate::config::WindowAppearance::Auto;
        config
            .profiles
            .insert("dark".into(), crate::config::ProfileConfig::default());
        let mut f = FollowOsAppearance::from_config(&config).expect("auto + profile → Some");
        let t0 = std::time::Instant::now();
        f.next_poll = t0 + APPEARANCE_POLL_INTERVAL;
        // Before the interval elapses the OS is never consulted, so no
        // edge can be reported no matter what the host's appearance is.
        assert_eq!(f.poll(t0), None);
        assert_eq!(f.poll(t0 + APPEARANCE_POLL_INTERVAL / 2), None);
        // Past the interval the throttle re-arms (the returned value
        // depends on the host appearance, so assert the SCHEDULE).
        let t1 = t0 + APPEARANCE_POLL_INTERVAL;
        let _ = f.poll(t1);
        assert_eq!(f.next_poll, t1 + APPEARANCE_POLL_INTERVAL);
    }

    #[test]
    fn appearance_profile_names_match_the_shipped_profile_example() {
        // ProfileConfig's own doc-comment uses `profiles: { light: … }`;
        // these names must keep matching it or an operator following the
        // documented example gets nothing.
        assert_eq!(appearance_profile(false), "light");
        assert_eq!(appearance_profile(true), "dark");
    }

    #[test]
    fn native_styling_latch_ticks_are_safe_and_idempotent() {
        let style = MacOsWindowStyle {
            native_tabs: false,
            titlebar: crate::config::TitlebarStyle::default(),
            appearance: crate::config::WindowAppearance::default(),
            background: ishou_tokens::Srgb::new(0x2e, 0x34, 0x40),
        };
        // Off the main thread the latch must keep retrying (macOS) or
        // settle immediately (elsewhere) — and never panic either way.
        std::thread::spawn(move || {
            let mut latch = NativeStylingLatch {
                style,
                applied: false,
            };
            latch.tick();
            latch.tick();
            #[cfg(target_os = "macos")]
            assert!(
                !latch.applied,
                "off-main-thread ticks must keep the latch armed"
            );
            #[cfg(not(target_os = "macos"))]
            assert!(latch.applied);
        })
        .join()
        .unwrap();
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
        NSApplication, NSColor, NSMenu, NSMenuItem, NSRequestUserAttentionType, NSScreen,
        NSTitlebarSeparatorStyle, NSWindow, NSWindowStyleMask, NSWindowTabbingMode,
        NSWindowTitleVisibility,
    };
    use objc2_foundation::{NSPoint, NSProcessInfo, NSRect, NSSize, NSString, NSUserDefaults};

    /// Build and install mado's menubar. See `super::install_app_menu` for
    /// the reasoning; this is the mechanism only.
    pub(super) fn install_app_menu() {
        use objc2::sel;
        use objc2_foundation::ns_string;

        // Off the main thread there is no legal way to touch AppKit, and a
        // missing menu is far better than a crash — so this is a silent
        // no-op rather than an unwrap.
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);

        let menubar = NSMenu::new(mtm);
        let app_item = NSMenuItem::new(mtm);
        menubar.addItem(&app_item);

        let app_menu = NSMenu::new(mtm);
        let name = NSProcessInfo::processInfo().processName();

        let item = |title: &NSString, action, key: &NSString| unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), title, action, key)
        };

        let about = item(
            &ns_string!("About ").stringByAppendingString(&name),
            Some(sel!(orderFrontStandardAboutPanel:)),
            ns_string!(""),
        );
        let hide = item(
            &ns_string!("Hide ").stringByAppendingString(&name),
            Some(sel!(hide:)),
            ns_string!("h"),
        );
        let quit = item(
            &ns_string!("Quit ").stringByAppendingString(&name),
            Some(sel!(terminate:)),
            ns_string!("q"),
        );

        app_menu.addItem(&about);
        app_menu.addItem(&NSMenuItem::separatorItem(mtm));
        app_menu.addItem(&hide);
        app_menu.addItem(&NSMenuItem::separatorItem(mtm));
        app_menu.addItem(&quit);
        app_item.setSubmenu(Some(&app_menu));

        // Deliberately NOT `setServicesMenu` — mado implements no
        // `NSServicesMenuRequestor`, so a Services submenu here could not
        // reach the terminal selection. Adding one back without the
        // protocol would restore exactly the dead menu this replaced.
        app.setMainMenu(Some(&menubar));
    }

    use crate::config::{TitlebarStyle, WindowAppearance};

    /// Apply macOS-specific window styling from the operator's shikumi
    /// config. Pure safe Rust via objc2 bindings — zero raw FFI. Every
    /// branch below is driven by a `MacOsWindowStyle` field, which the
    /// operator authors under `window.macos.*` / `appearance.background`
    /// in `~/.config/mado/mado.yaml`. Defaults bias to "just the
    /// terminal": flush titlebar, no native tabs, dark appearance.
    ///
    /// Styles EVERY app window, not just the key one — at the first
    /// redraw the window often isn't key yet (`keyWindow()` is nil
    /// during launch), which used to silently skip styling. Returns
    /// `true` once at least one window was styled so callers can
    /// retry until the window materializes.
    pub fn apply_styling(style: &super::MacOsWindowStyle) -> bool {
        // Off the main thread, AppKit is untouchable — report
        // not-applied (part of the false/retry contract) so a
        // main-thread tick can pick it up.
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("apply_styling called off main thread — deferred");
            return false;
        };

        let app = NSApplication::sharedApplication(mtm);

        let windows = app.windows();
        if windows.count() == 0 {
            tracing::trace!("no app windows yet — styling deferred to a later tick");
            return false;
        }

        // ── Native window tabbing (window.macos.native_tabs) ─────────
        // The macOS-native tab bar — the `⌘1 / ⌘2 / …` tab strip plus the
        // `+` new-tab button that render as a grey band under the titlebar
        // — is redundant chrome by default: mado owns sessions, panes, and
        // windows through its integrated `tear` runtime. Default-off
        // disallows it globally (the strip + `+` never appear, ghostty's
        // behaviour); the per-window mode is set in the loop below.
        // Operators can opt back into the OS tab bar with
        // `native_tabs: true`.
        NSWindow::setAllowsAutomaticWindowTabbing(style.native_tabs, mtm);

        for window in windows.iter() {
            style_window(&window, style);
        }

        tracing::debug!(
            native_tabs = style.native_tabs,
            titlebar = ?style.titlebar,
            appearance = ?style.appearance,
            windows = windows.count(),
            "applied macOS native window styling from config"
        );
        true
    }

    /// Per-window half of [`apply_styling`] — titlebar integration,
    /// per-window tabbing mode, and forced appearance.
    fn style_window(window: &NSWindow, style: &super::MacOsWindowStyle) {
        // ── Titlebar integration (window.macos.titlebar) ─────────────
        match style.titlebar {
            TitlebarStyle::Flush | TitlebarStyle::Overlay => {
                // Shared themed chrome: transparent titlebar + hidden
                // title + no hairline separator + drag-from-anywhere,
                // over a window backing tinted to the terminal
                // background — the band reads as part of the canvas.
                //
                // The two styles differ in ONE bit: Overlay inserts
                // FullSizeContentView so the cell grid extends under
                // the floating traffic lights (max canvas, top row
                // overlapped); Flush removes it so the grid starts
                // below the button strip and text is never covered
                // (ghostty's transparent look).
                let mut mask = window.styleMask();
                if style.titlebar == TitlebarStyle::Overlay {
                    mask.insert(NSWindowStyleMask::FullSizeContentView);
                } else {
                    mask.remove(NSWindowStyleMask::FullSizeContentView);
                }
                window.setStyleMask(mask);
                window.setTitlebarAppearsTransparent(true);
                window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
                window.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None);
                // Deliberately NOT movable-by-window-background: that
                // flag made every content-area drag a WINDOW drag, so
                // selecting text also slid the window around (operator
                // report 2026-06-11). The drag/select split is the
                // titlebar contract (iTerm2/ghostty): the themed band
                // — where the traffic lights live — drags the window
                // natively (the NSTitlebarContainerView keeps handling
                // drags in both Flush and Overlay, floating above the
                // content view); the cell grid below it selects text
                // and never moves the window.
                window.setMovableByWindowBackground(false);

                // Tint the NSWindow backing to the configured terminal
                // background so the titlebar band matches the cell grid
                // instead of the macOS default. The GPU surface renders
                // opaque content over the backing, so a flush same-colour
                // band is the seamless result.
                let r = f64::from(style.background.r) / 255.0;
                let g = f64::from(style.background.g) / 255.0;
                let b = f64::from(style.background.b) / 255.0;
                let bg = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
                window.setBackgroundColor(Some(&bg));
            }
            TitlebarStyle::Native => {
                // Leave the stock macOS titlebar untouched — operators
                // who set `titlebar: native` want a conventional Mac
                // window frame (opaque band, separator, visible title).
                tracing::debug!("titlebar: native — leaving stock macOS chrome");
            }
        }

        // ── Per-window tabbing mode (window.macos.native_tabs) ───────
        // Belt-and-suspenders next to the app-global
        // `setAllowsAutomaticWindowTabbing` in `apply_styling` — covers
        // a window created before the global flag landed.
        if style.native_tabs {
            window.setTabbingMode(NSWindowTabbingMode::Automatic);
        } else {
            window.setTabbingMode(NSWindowTabbingMode::Disallowed);
        }

        // ── Forced appearance (window.macos.appearance) ──────────────
        // Without a forced appearance, macOS renders the residual
        // titlebar-container material in the *system* appearance — a
        // translucent light fill that reads as a lighter-grey band over a
        // dark background. `Dark` (the default) makes that material dark
        // so the chrome is flush and the traffic-light glyphs render in
        // dark mode; `Light` forces light; `Auto` follows the system.
        let forced = match style.appearance {
            WindowAppearance::Dark => {
                NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua })
            }
            WindowAppearance::Light => {
                NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua })
            }
            // `None` resets the window to inherit the system appearance.
            WindowAppearance::Auto => None,
        };
        window.setAppearance(forced.as_deref());
    }

    /// Bounce the dock until the app gains focus (OSC 1337
    /// `RequestAttention`). `NSCriticalRequest` keeps bouncing until
    /// the operator responds — matching the escape's "needs the
    /// human" intent. Off the main thread this is a deferred no-op
    /// (the drain consumer runs on the event-loop thread, which IS
    /// the main thread; the guard covers tests).
    pub fn request_dock_attention() {
        request_attention(true);
    }

    /// Request attention at a typed level. `CriticalRequest` bounces the
    /// dock until focus returns; `InformationalRequest` bounces once.
    pub fn request_attention(critical: bool) {
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::debug!("request_attention off main thread — skipped");
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        let level = if critical {
            NSRequestUserAttentionType::CriticalRequest
        } else {
            NSRequestUserAttentionType::InformationalRequest
        };
        let _token = app.requestUserAttention(level);
    }

    /// Ring the audible bell: `None` → the classic system beep
    /// (`NSBeep`); `Some(name)` → a named `NSSound`, falling back to
    /// `NSBeep` when the name is unknown. Playback is async + thread-safe;
    /// in practice called from the event-loop (main) thread.
    pub fn ring_bell(sound: Option<&str>) {
        use objc2_app_kit::{NSBeep, NSSound};
        match sound {
            None => NSBeep(),
            Some(name) => {
                let ns = NSString::from_str(name);
                match NSSound::soundNamed(&ns) {
                    Some(s) => {
                        s.play();
                    }
                    None => {
                        tracing::debug!(
                            sound = name,
                            "bell sound not found — falling back to NSBeep"
                        );
                        NSBeep();
                    }
                }
            }
        }
    }

    /// tsuuchi [`NotificationBackend`](tsuuchi::NotificationBackend)
    /// delivering through `osascript`'s `display notification` — the
    /// path that works for an unbundled binary (the modern
    /// `UNUserNotificationCenter` API aborts without a bundle
    /// identifier, and `NSUserNotificationCenter` returns nil there).
    ///
    /// The `AppleScript` program is a CONSTANT `on run argv` script;
    /// user-controlled strings (title/body) travel as argv items and
    /// are never spliced into source — `AppleScript` injection has no
    /// representation. Urgency and group have no `display
    /// notification` surface; they are traced so the typed value is
    /// still observable (honest partial mapping, never silently
    /// dropped).
    pub struct OsaScriptBackend;

    /// Constant notification program — argv item 1 = title, item 2 =
    /// body. No interpolation by construction.
    const NOTIFY_SCRIPT: [&str; 3] = [
        "on run argv",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "end run",
    ];

    impl tsuuchi::NotificationBackend for OsaScriptBackend {
        fn send(&self, notification: &tsuuchi::Notification) -> Result<(), tsuuchi::TsuuchiError> {
            tracing::debug!(
                title = %notification.title,
                urgency = ?notification.urgency,
                group = ?notification.group,
                "dispatching macOS notification via osascript (urgency/group have no display-notification surface)"
            );
            let mut cmd = std::process::Command::new("/usr/bin/osascript");
            for line in NOTIFY_SCRIPT {
                cmd.arg("-e").arg(line);
            }
            let child = cmd
                .arg(&notification.title)
                .arg(&notification.body)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| tsuuchi::TsuuchiError::Unavailable(e.to_string()))?;
            // Detached reap: never block the render loop on the
            // notification daemon; a reaper thread prevents zombies.
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
            Ok(())
        }
    }

    /// Set dock badge text.
    pub fn set_badge(text: Option<&str>) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };

        let app = NSApplication::sharedApplication(mtm);
        let dock_tile = app.dockTile();

        let label = text.map(|t| NSString::from_str(t));
        dock_tile.setBadgeLabel(label.as_deref());
    }

    /// Place + show (or order out) every app window as the Quick
    /// Terminal, per the operator's `quick_terminal.*` config.
    ///
    /// The geometry is NOT computed here: `resolve_size_pixels` /
    /// `resolve_origin_pixels` are the config's own shipped resolvers,
    /// and the top-left→Cocoa flip is [`super::cocoa_frame`], so the
    /// only thing this function owns is the AppKit call. Returns `true`
    /// once it reached a real window (the retry contract
    /// [`super::QuickTerminal::tick`] drives).
    pub fn quick_terminal_apply(cfg: &crate::config::QuickTerminalConfig, visible: bool) -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("quick_terminal_apply called off main thread — deferred");
            return false;
        };
        let app = NSApplication::sharedApplication(mtm);
        let windows = app.windows();
        if windows.count() == 0 {
            tracing::trace!("no app windows yet — quick terminal placement deferred");
            return false;
        }

        if !visible {
            for window in windows.iter() {
                window.orderOut(None);
            }
            return true;
        }

        let Some(screen) = NSScreen::mainScreen(mtm) else {
            tracing::warn!("no main screen — quick terminal placement deferred");
            return false;
        };
        // visibleFrame, not frame: it already excludes the menu bar and
        // the Dock, which is the box a drop-down should pin to.
        let vf = screen.visibleFrame();
        let visible_rect = super::ScreenRect {
            x: vf.origin.x,
            y: vf.origin.y,
            w: vf.size.width,
            h: vf.size.height,
        };
        // The config resolvers speak u32 pixels; clamp at 1 so a
        // degenerate screen report cannot produce a zero-size window.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let screen_px = (
            visible_rect.w.max(1.0) as u32,
            visible_rect.h.max(1.0) as u32,
        );
        let size = cfg.resolve_size_pixels(screen_px);
        let top_left = cfg.resolve_origin_pixels(screen_px);
        let frame = super::cocoa_frame(visible_rect, size, top_left);
        let rect = NSRect::new(
            NSPoint::new(frame.x, frame.y),
            NSSize::new(frame.w, frame.h),
        );

        // `animation_ms` maps to a BOOLEAN here, honestly and partially:
        // zero snaps, non-zero animates. AppKit owns the duration
        // (`NSWindow::animationResizeTime` is an override point, not a
        // setter), so the operator's exact millisecond count is not
        // reachable through this API — a precise duration would need an
        // NSAnimationContext-driven frame animation. Documented rather
        // than silently rounded.
        let animate = cfg.animation_ms > 0;
        for window in windows.iter() {
            window.setFrame_display_animate(rect, true, animate);
            window.makeKeyAndOrderFront(None);
        }
        tracing::debug!(
            edge = ?cfg.edge,
            size_fraction = cfg.size_fraction,
            animate,
            w = size.0,
            h = size.1,
            "quick terminal placed"
        );
        true
    }

    /// Check if macOS is in dark mode.
    pub fn is_dark_mode() -> bool {
        let defaults = NSUserDefaults::standardUserDefaults();

        let Some(value) = defaults.stringForKey(&NSString::from_str("AppleInterfaceStyle")) else {
            return false; // No AppleInterfaceStyle = light mode
        };

        value.isEqualToString(&NSString::from_str("Dark"))
    }
}
