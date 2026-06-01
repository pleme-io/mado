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

/// Apply platform-native window styling from the operator config.
/// On macOS this drives titlebar integration, native-tab suppression,
/// and forced appearance — all shikumi-configured; a no-op elsewhere.
pub fn apply_native_styling(style: &MacOsWindowStyle) {
    #[cfg(target_os = "macos")]
    macos::apply_styling(style);

    #[cfg(not(target_os = "macos"))]
    {
        let _ = style;
        tracing::debug!("no platform-specific styling for this OS");
    }
}

/// Set the macOS dock icon badge text (e.g., for bell notifications).
#[allow(dead_code)]
pub fn set_badge(_text: Option<&str>) {
    #[cfg(target_os = "macos")]
    macos::set_badge(_text);
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
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
        NSApplication, NSColor, NSTitlebarSeparatorStyle, NSWindow, NSWindowStyleMask,
        NSWindowTabbingMode, NSWindowTitleVisibility,
    };
    use objc2_foundation::{NSString, NSUserDefaults};

    use crate::config::{TitlebarStyle, WindowAppearance};

    /// Apply macOS-specific window styling from the operator's shikumi
    /// config. Pure safe Rust via objc2 bindings — zero raw FFI. Every
    /// branch below is driven by a `MacOsWindowStyle` field, which the
    /// operator authors under `window.macos.*` / `appearance.background`
    /// in `~/.config/mado/mado.yaml`. Defaults bias to "just the
    /// terminal": flush titlebar, no native tabs, dark appearance.
    pub fn apply_styling(style: &super::MacOsWindowStyle) {
        // We're called from the main event loop, so main thread is guaranteed.
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("apply_styling called off main thread");
            return;
        };

        let app = NSApplication::sharedApplication(mtm);

        let Some(window) = app.keyWindow() else {
            tracing::trace!("no key window for styling");
            return;
        };

        // ── Titlebar integration (window.macos.titlebar) ─────────────
        match style.titlebar {
            TitlebarStyle::Flush => {
                // FullSizeContentView + transparent titlebar + hidden
                // title + no hairline separator + drag-from-anywhere:
                // the cell grid runs flush to the window's top edge and
                // the traffic lights float over it (ghostty's look).
                let mut mask = window.styleMask();
                mask.insert(NSWindowStyleMask::FullSizeContentView);
                window.setStyleMask(mask);
                window.setTitlebarAppearsTransparent(true);
                window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
                window.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None);
                window.setMovableByWindowBackground(true);

                // Tint the NSWindow backing to the configured terminal
                // background so the titlebar band matches the cell grid
                // instead of the macOS default. The GPU surface renders
                // opaque content over the backing, so a flush same-colour
                // band is the seamless result.
                let r = f64::from(style.background.r) / 255.0;
                let g = f64::from(style.background.g) / 255.0;
                let b = f64::from(style.background.b) / 255.0;
                let bg = unsafe { NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0) };
                window.setBackgroundColor(Some(&bg));
            }
            TitlebarStyle::Native => {
                // Leave the stock macOS titlebar untouched — operators
                // who set `titlebar: native` want a conventional Mac
                // window frame (opaque band, separator, visible title).
                tracing::debug!("titlebar: native — leaving stock macOS chrome");
            }
        }

        // ── Native window tabbing (window.macos.native_tabs) ─────────
        // The macOS-native tab bar — the `⌘1 / ⌘2 / …` tab strip plus the
        // `+` new-tab button that render as a grey band under the titlebar
        // — is redundant chrome by default: mado owns sessions, panes, and
        // windows through its integrated `tear` runtime. Default-off
        // disallows it globally (the strip + `+` never appear, ghostty's
        // behaviour) and per-window as belt-and-suspenders for the already
        // -created window that predated the global flag. Operators can opt
        // back into the OS tab bar with `native_tabs: true`.
        if style.native_tabs {
            NSWindow::setAllowsAutomaticWindowTabbing(true, mtm);
            window.setTabbingMode(NSWindowTabbingMode::Automatic);
        } else {
            NSWindow::setAllowsAutomaticWindowTabbing(false, mtm);
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

        tracing::debug!(
            native_tabs = style.native_tabs,
            titlebar = ?style.titlebar,
            appearance = ?style.appearance,
            "applied macOS native window styling from config"
        );
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
