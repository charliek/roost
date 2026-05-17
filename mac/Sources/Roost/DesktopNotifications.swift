// Desktop notifications — Phase 6a P8.
//
// Routes daemon-emitted `NotificationEvent`s (Phase 6b OSC 9/777 +
// `roost-cli-rs notify` paths) to native macOS banners via
// `UNUserNotificationCenter`. First-launch authorization prompt
// fires from `applicationDidFinishLaunching` so the user gets the
// permission dialog at a predictable moment rather than mid-session
// when the first notification would otherwise trigger it.
//
// Click handler — when the user clicks a banner, AppKit raises
// Roost (the system already does this), and the
// `UNUserNotificationCenterDelegate` callback runs on the main
// actor with the tab id pulled out of `userInfo`. P8 walks the
// daemon's `tabs` list to find the matching tab + project, then
// reuses M2's `selectProject` + M3's `selectTab` paths to focus
// it.
//
// Out of scope (separate slices):
//   * Notification grouping (UNNotificationContent supports
//     `threadIdentifier`; we'd want one per project so banners
//     coalesce in Notification Center). Lands in a polish pass
//     once dogfooding confirms which grouping users want.
//   * Sound / banner-style preferences. We use the OS defaults;
//     users tweak in System Settings.

import AppKit
import Foundation
import UserNotifications

/// Identifier the click handler matches on. Future iterations may
/// add per-event-class categories (success / error / etc.); P8
/// uses a single `"roost-tab"` category for the simple case.
private let roostTabCategoryID = "roost-tab"

/// Key under which we store the daemon `tab_id` on the notification's
/// `userInfo` payload. The click handler reads it back to know which
/// tab to focus.
private let tabIDUserInfoKey = "roost.tab_id"

/// Singleton-style coordinator for UN Notification Center on the
/// Swift app side. Owns the delegate (which has to be retained for
/// the click callbacks to keep firing) + the authorization flag.
/// `RoostApp` holds one of these in a property.
@MainActor
final class DesktopNotifications: NSObject, UNUserNotificationCenterDelegate {
    /// Called when the user clicks a notification banner (or
    /// expands one in Notification Center). Receives the tab id
    /// payloaded into `userInfo`. RoostApp wires this to walk its
    /// `projects` + `tabs` and focus the matching one.
    var onActivate: ((Int64) -> Void)?

    private let center: UNUserNotificationCenter
    private var authorized: Bool = false

    override init() {
        self.center = UNUserNotificationCenter.current()
        super.init()
        self.center.delegate = self
        // Register the category so click actions route through
        // our delegate. M8's spike skipped this; banners would
        // still display but the click would just dismiss.
        let category = UNNotificationCategory(
            identifier: roostTabCategoryID,
            actions: [],
            intentIdentifiers: [],
            options: []
        )
        self.center.setNotificationCategories([category])
    }

    /// Ask the user for notification permissions. Triggered from
    /// `applicationDidFinishLaunching` so the dialog arrives early
    /// — better UX than blocking the first real notification on
    /// authorization. macOS persists the user's answer across
    /// launches via the bundle id; subsequent calls no-op if
    /// already authorized or denied.
    func requestAuthorization() {
        center.requestAuthorization(options: [.alert, .sound, .badge]) { [weak self] granted, error in
            if let error {
                NSLog("roost-mac: notification authorization error: %@", "\(error)")
            }
            // The callback is delivered on an arbitrary queue —
            // hop back to main before touching state.
            Task { @MainActor [weak self] in
                self?.authorized = granted
            }
        }
    }

    /// Fire a notification for one `NotificationEvent`. No-op if
    /// the user denied authorization at the prompt — better to
    /// silently drop than spam the system console with
    /// "no banner shown" errors.
    func emit(tabID: Int64, title: String, body: String) {
        guard authorized else { return }
        let content = UNMutableNotificationContent()
        content.title = title.isEmpty ? "Roost" : title
        content.body = body
        content.categoryIdentifier = roostTabCategoryID
        content.userInfo = [tabIDUserInfoKey: tabID]
        // Identifier uses tab id + timestamp so banners don't
        // coalesce by accident; users that want grouping can
        // configure it in System Settings. Unique-per-event keeps
        // multiple notifications visible at once.
        let identifier = "roost-tab-\(tabID)-\(Int(Date().timeIntervalSince1970 * 1000))"
        let request = UNNotificationRequest(
            identifier: identifier,
            content: content,
            trigger: nil  // fire immediately
        )
        center.add(request) { error in
            if let error {
                NSLog("roost-mac: notification add failed: %@", "\(error)")
            }
        }
    }

    // MARK: - UNUserNotificationCenterDelegate

    /// Required to make banners visible while the app is in the
    /// foreground. Without this, macOS suppresses banners when the
    /// originating app is frontmost — which is exactly the case
    /// where a Claude session in a Roost tab triggers one. Return
    /// `.banner` + `.sound` so banners fire regardless of frontmost.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        completionHandler([.banner, .sound])
    }

    /// User clicked the banner. Pull the tab id out of `userInfo`
    /// and hand to `onActivate` on the main actor — RoostApp's
    /// installed callback walks its model + focuses the tab.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let userInfo = response.notification.request.content.userInfo
        if let raw = userInfo[tabIDUserInfoKey] as? Int64 {
            Task { @MainActor [weak self] in
                self?.onActivate?(raw)
            }
        } else if let raw = userInfo[tabIDUserInfoKey] as? Int {
            // JSON decoding can sometimes round-trip Int64 as Int
            // when the value fits — handle both.
            Task { @MainActor [weak self] in
                self?.onActivate?(Int64(raw))
            }
        }
        completionHandler()
    }
}
