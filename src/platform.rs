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
#[derive(Debug, Clone)]
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
                if let Some(un) = crate::notify_mac::UnBackend::try_new() {
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
            assert!(!latch.applied, "off-main-thread ticks must keep the latch armed");
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
        NSApplication, NSColor, NSRequestUserAttentionType, NSTitlebarSeparatorStyle, NSWindow,
        NSWindowStyleMask, NSWindowTabbingMode, NSWindowTitleVisibility,
    };
    use objc2_foundation::{NSString, NSUserDefaults};

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
                        tracing::debug!(sound = name, "bell sound not found — falling back to NSBeep");
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
        fn send(
            &self,
            notification: &tsuuchi::Notification,
        ) -> Result<(), tsuuchi::TsuuchiError> {
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

    /// Check if macOS is in dark mode.
    pub fn is_dark_mode() -> bool {
        let defaults = NSUserDefaults::standardUserDefaults();

        let Some(value) = defaults.stringForKey(&NSString::from_str("AppleInterfaceStyle")) else {
            return false; // No AppleInterfaceStyle = light mode
        };

        value.isEqualToString(&NSString::from_str("Dark"))
    }
}
