//! Native macOS notifications via `UNUserNotificationCenter`.
//!
//! Replaces the legacy `osascript display notification` shell-out — which
//! macOS attributed to *Script Editor* and which tripped the automation
//! permission popup. This backend delivers **mado-attributed** banners
//! with sound, an `Urgency → interruption-level` mapping, and thread
//! grouping.
//!
//! It is only viable when mado runs **bundled** (`Mado.app`, bundle id
//! `io.pleme.mado`): `UNUserNotificationCenter` aborts for a process with
//! no bundle identifier. [`UnBackend::try_new`] returns `None` when
//! unbundled so the caller falls back (dock/log) — never the popup.
//!
//! Threading: `send` builds every Objective-C object locally and never
//! stores a `Retained` across threads, so [`UnBackend`] is a ZST and
//! trivially `Send + Sync`. `UNUserNotificationCenter` is documented safe
//! to message from any thread.
//!
//! Tier honesty (see `docs/NOTIFICATIONS.md`): v1 delivers content +
//! sound + interruption level + thread + stable-id (update-in-place).
//! Action buttons and attachments travel in the tsuuchi vocabulary but
//! are **not yet routed** — the `UNUserNotificationCenterDelegate`
//! response path is M1. Unsupported axes are traced, never dropped.

use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSProcessInfo, NSString, NSUUID};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationInterruptionLevel,
    UNNotificationRequest, UNNotificationSound, UNUserNotificationCenter,
};
use std::sync::Once;
use tsuuchi::{Capabilities, Notification, NotificationSound, TsuuchiError, Urgency};

/// Native `UNUserNotificationCenter` backend. Zero-sized: it stores no
/// `Retained` handle (each `send` fetches the shared center), so it is
/// `Send + Sync` and satisfies [`tsuuchi::NotificationBackend`].
pub struct UnBackend;

static AUTH_ONCE: Once = Once::new();

impl UnBackend {
    /// Construct the native backend, or `None` when mado is not running
    /// bundled (no `CFBundleIdentifier` → `UNUserNotificationCenter`
    /// would abort). On success, requests authorization exactly once per
    /// process.
    #[must_use]
    pub fn try_new() -> Option<Self> {
        // The single load-bearing precondition: a bundle identifier.
        let bundled = NSBundle::mainBundle().bundleIdentifier().is_some();
        if !bundled {
            tracing::info!(
                "mado is not bundled (no CFBundleIdentifier) — native \
                 UNUserNotificationCenter unavailable; falling back"
            );
            return None;
        }
        AUTH_ONCE.call_once(request_authorization);
        Some(Self)
    }
}

/// Request Alert+Sound+Badge authorization once per process. The macOS
/// prompt is the normal "Mado would like to send you notifications",
/// attributed to mado — not the Script-Editor automation popup.
fn request_authorization() {
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let opts = UNAuthorizationOptions::Alert
        | UNAuthorizationOptions::Sound
        | UNAuthorizationOptions::Badge;
    let handler = block2::RcBlock::new(|granted: Bool, _err: *mut NSError| {
        tracing::debug!(granted = granted.as_bool(), "UN authorization result");
    });
    unsafe {
        center.requestAuthorizationWithOptions_completionHandler(opts, &handler);
    }
}

/// `setInterruptionLevel:` requires macOS 12+; on 11 the selector is
/// unrecognised (would abort). Gate on the running OS major version.
fn supports_interruption_level() -> bool {
    NSProcessInfo::processInfo().operatingSystemVersion().majorVersion >= 12
}

/// Map tsuuchi urgency to a macOS interruption level. `Critical` maps to
/// `TimeSensitive` (pierces Focus, no entitlement) rather than the true
/// `Critical` level (which needs an Apple entitlement).
fn interruption_level(urgency: Urgency) -> UNNotificationInterruptionLevel {
    match urgency {
        Urgency::Low => UNNotificationInterruptionLevel::Passive,
        Urgency::Normal => UNNotificationInterruptionLevel::Active,
        Urgency::Critical => UNNotificationInterruptionLevel::TimeSensitive,
    }
}

/// Resolve a tsuuchi sound to a `UNNotificationSound` (or `None` for
/// silent).
fn sound_for(sound: &NotificationSound) -> Option<Retained<UNNotificationSound>> {
    match sound {
        NotificationSound::Silent => None,
        NotificationSound::Default => Some(unsafe { UNNotificationSound::defaultSound() }),
        NotificationSound::Critical => {
            Some(unsafe { UNNotificationSound::defaultCriticalSound() })
        }
        NotificationSound::Named(name) => {
            let ns = NSString::from_str(name);
            Some(unsafe { UNNotificationSound::soundNamed(&ns) })
        }
    }
}

impl tsuuchi::NotificationBackend for UnBackend {
    fn send(&self, n: &Notification) -> Result<(), TsuuchiError> {
        // Honest partial mapping — carried in the vocabulary, not yet
        // delivered on the native path (M1).
        if !n.actions.is_empty() {
            tracing::debug!(
                count = n.actions.len(),
                "UN backend: actions carried but not yet routed (M1)"
            );
        }
        if n.attachment.is_some() {
            tracing::debug!("UN backend: attachment carried but not yet delivered (M1)");
        }

        let content = unsafe { UNMutableNotificationContent::new() };
        content.setTitle(&NSString::from_str(&n.title));
        content.setBody(&NSString::from_str(&n.body));
        if let Some(sub) = &n.subtitle {
            content.setSubtitle(&NSString::from_str(sub));
        }
        content.setSound(sound_for(&n.sound).as_deref());
        if let Some(group) = &n.group {
            content.setThreadIdentifier(&NSString::from_str(group));
        }
        if let Some(category) = &n.category {
            content.setCategoryIdentifier(&NSString::from_str(category));
        }
        if supports_interruption_level() {
            content.setInterruptionLevel(interruption_level(n.urgency));
        }

        // Stable id → update-in-place; otherwise a fresh UUID per send.
        let identifier = match &n.id {
            Some(id) => NSString::from_str(id),
            None => unsafe { NSUUID::UUID() }.UUIDString(),
        };
        let request = unsafe {
            UNNotificationRequest::requestWithIdentifier_content_trigger(&identifier, &content, None)
        };

        let center = UNUserNotificationCenter::currentNotificationCenter();
        unsafe {
            center.addNotificationRequest_withCompletionHandler(&request, None);
        }
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            actions: false, // M1: delegate response routing
            sound: true,
            attachments: false, // M1
            interruption_levels: supports_interruption_level(),
            reply: false, // M1
            update_in_place: true,
        }
    }
}
