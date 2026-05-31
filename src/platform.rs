//! Platform-specific integration.
//!
//! macOS: window styling via objc2 safe bindings (transparent titlebar, native appearance).
//! Linux: placeholder for Wayland-specific integration.

/// Apply platform-native window styling.
/// On macOS, this configures the titlebar to be transparent and integrated.
pub fn apply_native_styling() {
    #[cfg(target_os = "macos")]
    macos::apply_styling();

    #[cfg(not(target_os = "macos"))]
    tracing::debug!("no platform-specific styling for this OS");
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
        NSApplication, NSColor, NSTitlebarSeparatorStyle, NSWindowStyleMask,
        NSWindowTitleVisibility,
    };
    use objc2_foundation::{NSString, NSUserDefaults};

    /// Nord polar-night background (`#2E3440`) — the canonical
    /// pleme-io GUI app window color. Operators can override via
    /// `config.appearance.background` (the renderer uses that for
    /// the cell-grid area), but the NSWindow backing store is
    /// always Nord so the macOS titlebar tint inherits the same
    /// color and the brown/cream macOS default never shows through.
    const NORD_POLAR_NIGHT: (f64, f64, f64, f64) =
        (0x2E as f64 / 255.0, 0x34 as f64 / 255.0, 0x40 as f64 / 255.0, 1.0);

    /// Apply macOS-specific window styling.
    /// Pure safe Rust via objc2 bindings — zero raw FFI.
    pub fn apply_styling() {
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

        // Set titlebar appearance: transparent + full-size content view
        let mut mask = window.styleMask();
        mask.insert(NSWindowStyleMask::FullSizeContentView);
        window.setStyleMask(mask);

        // Make titlebar transparent
        window.setTitlebarAppearsTransparent(true);

        // Set title visibility to hidden — belt + suspenders with the
        // empty title string callers usually set.
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);

        // Remove the macOS 11+ hairline separator macOS draws under the
        // titlebar. With FullSizeContentView + a transparent titlebar +
        // the same Nord backing colour, that separator was the one thing
        // making the titlebar read as a distinct band rather than a
        // seamless extension of the content — the "doesn't blend like
        // ghostty" symptom. `.None` removes it so the titlebar is flush
        // with the cell grid below (ghostty's flush look).
        window.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None);

        // Drag the window from anywhere in the (seamless) titlebar/content
        // band, matching ghostty's integrated chrome feel. Safe because
        // the GPU surface consumes mouse events for selection only inside
        // the content rect; the titlebar band has no interactive cells.
        window.setMovableByWindowBackground(true);

        // Tint the NSWindow backing to Nord polar-night so the titlebar
        // area inherits the pleme-io palette instead of the macOS
        // default (which the operator sees as "brown" against the
        // Nord content area). The GPU surface renders opaque content
        // over the backing, so a true vibrancy/blur (NSVisualEffectView)
        // would never show through — a flush, same-colour titlebar is
        // the correct seamless result here. Snowflake glyph in the
        // (hidden) title is the brand mark; if anything peeks through
        // it's Nord.
        let (r, g, b, a) = NORD_POLAR_NIGHT;
        let bg = unsafe { NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a) };
        window.setBackgroundColor(Some(&bg));

        tracing::debug!(
            "applied macOS native window styling (flush nord titlebar, no separator)"
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
