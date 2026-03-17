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
    use objc2_app_kit::{NSApplication, NSWindowStyleMask, NSWindowTitleVisibility};
    use objc2_foundation::{NSString, NSUserDefaults};

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

        // Set title visibility to hidden
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);

        tracing::debug!("applied macOS native window styling");
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
