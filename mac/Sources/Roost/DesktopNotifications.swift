// Desktop notifications — Phase 6a P8.
//
// Routes daemon-emitted `NotificationEvent`s (Phase 6b OSC 9/777 +
// `roost-cli-rs notify` paths) to native macOS banners via
// `UNUserNotificationCenter`. First-launch authorization prompt
// fires from `applicationDidFinishLaunching` so the user gets the
// permission dialog at a predictable moment rather than mid-session
// when the first notification would otherwise trigger it. The answer
// is re-read from the system by `refreshAuthorization()` on every
// activation, because it can change under a running app (the user
// denies, grants in System Settings, and comes back) — the iced twin
// is `crates/roost-iced/src/macos/notifications.rs`.
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

/// The cached "may we post banners" bit, plus the ticket that says who
/// is allowed to write it.
///
/// Two writers ask the system at different moments —
/// `requestAuthorization()` and `refreshAuthorization()` — and UN
/// completes both on unspecified queues with no documented ordering
/// between separately issued queries. Without a guard, a delayed
/// `denied` snapshot can land after a newer `authorized` one and mute
/// notifications for the rest of the session, which is the very
/// staleness the refresh exists to remove. So each query carries the
/// generation it was issued at and only the newest may write; an older
/// in-flight snapshot is discarded, which is safe because a newer query
/// is by definition on its way with fresher truth.
///
/// A locked reference type rather than main-actor state, because the
/// ticket has to be claimed *where the answer arrives* — on UN's
/// completion queue. Claiming it after a hop to the main actor would
/// order the tickets by executor scheduling instead of by when the
/// answers came in (Swift promises no FIFO across unstructured tasks),
/// so a stale `denied` could take the newer ticket and overwrite a
/// `granted` that reached the main actor ahead of it. That is the same
/// bug the generation guard exists to prevent. The iced twin is an
/// `AtomicBool` + `AtomicU64`, touched from arbitrary queues for the
/// same reason.
///
/// It is standalone rather than a pair of stored properties on
/// `DesktopNotifications` so the sequencing is unit-testable —
/// constructing that class calls `UNUserNotificationCenter.current()`,
/// which aborts in an unbundled process such as the test runner.
final class NotificationAuthorization: @unchecked Sendable {
    /// Unchecked because the lock is hand-rolled: `state` is reachable
    /// only through the three accessors below, and every one of them
    /// holds `lock` for the whole access.
    private let lock = NSLock()
    private var state = (authorized: false, generation: 0)

    var authorized: Bool {
        lock.withLock { state.authorized }
    }

    /// Claim the next ticket, from wherever the caller's answer is at
    /// its freshest.
    func issueQuery() -> Int {
        lock.withLock {
            state.generation += 1
            return state.generation
        }
    }

    /// Record a completion's snapshot, unless a later query has since
    /// been issued. Returns whether the cached value actually moved, so
    /// the caller's transition log stays inside the guard — a discarded
    /// snapshot must not report a change it did not make.
    @discardableResult
    func store(generation: Int, authorized: Bool) -> Bool {
        lock.withLock {
            guard generation == state.generation, authorized != state.authorized else {
                return false
            }
            state.authorized = authorized
            return true
        }
    }
}

/// Write a completion's snapshot through the guard and log the
/// transition when the cached value moved. A free function so both
/// completion handlers call it directly on whatever queue UN chose,
/// with nothing between the answer arriving and its ticket being
/// claimed. `RoostLogger` is thread-safe (it serialises the file
/// appender on its own queue), so the log needs no hop either.
private func recordAuthorization(
    _ authorization: NotificationAuthorization, generation: Int, authorized: Bool
) {
    if authorization.store(generation: generation, authorized: authorized) {
        RoostLogger.shared.info(
            "notifications: authorization changed: \(!authorized) -> \(authorized)"
        )
    }
}

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
    private nonisolated let authorization = NotificationAuthorization()

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
    ///
    /// `nonisolated`, reaching the center through `current()` rather
    /// than the main-actor `center` property, because
    /// `refreshAuthorization()` re-issues it from UN's completion
    /// queue. `UNUserNotificationCenter` is documented safe from any
    /// thread, and `current()` returns the singleton `init` already
    /// took.
    nonisolated func requestAuthorization() {
        UNUserNotificationCenter.current().requestAuthorization(
            options: [.alert, .sound, .badge]
        ) { [authorization] granted, error in
            if let error {
                NSLog("roost-mac: notification authorization error: %@", "\(error)")
            }
            // The ticket is claimed here, in the completion and on the
            // queue UN answered on, not at issue time the way
            // `refreshAuthorization()` claims it: the asymmetry between
            // the two writers. A settings query is a snapshot and ages;
            // an authorization answer is an event, current the instant
            // it fires however long the prompt stood. Claiming now is
            // what stops a refresh issued while the prompt was up from
            // discarding the user's actual answer.
            recordAuthorization(
                authorization,
                generation: authorization.issueQuery(),
                authorized: granted
            )
        }
    }

    /// Re-read the system's answer. macOS can change it under a running
    /// app — the user denies at first launch, grants in System
    /// Settings, and comes back — and `applicationDidBecomeActive` is
    /// the moment that return happens. Re-issues the prompt only while
    /// the recorded answer is still `.notDetermined`; once macOS has an
    /// answer on file a repeat request is a silent no-op.
    func refreshAuthorization() {
        let generation = authorization.issueQuery()
        center.getNotificationSettings { [weak self, authorization] settings in
            let status = settings.authorizationStatus
            recordAuthorization(
                authorization,
                generation: generation,
                authorized: Self.isAllowed(status)
            )
            if status == .notDetermined {
                self?.requestAuthorization()
            }
        }
    }

    /// Only an explicit yes counts: `.denied`, `.notDetermined`, and any
    /// status this build does not know about all fall closed. macOS's
    /// SDK marks `.ephemeral` unavailable, so that one is unnameable
    /// here and reaches the unknown arm — which answers `false`, the
    /// verdict it would get anyway.
    nonisolated static func isAllowed(_ status: UNAuthorizationStatus) -> Bool {
        switch status {
        case .authorized, .provisional:
            return true
        case .notDetermined, .denied:
            return false
        @unknown default:
            return false
        }
    }

    /// The `app.notification_status` payload — the same
    /// `{backend, reason, authorized}` shape the iced UI serves
    /// (`AppNotificationStatusResult` in
    /// `crates/roost-ipc/src/messages.rs`). The delegate is installed in
    /// `init`, so a live `DesktopNotifications` is by construction an
    /// available backend: unlike iced, which also builds unbundled,
    /// this app would have aborted at launch otherwise.
    func status() -> (backend: String, reason: String?, authorized: Bool) {
        (backend: "available", reason: nil, authorized: authorization.authorized)
    }

    /// Fire a notification for one `NotificationEvent`. No-op if
    /// the user denied authorization at the prompt — better to
    /// silently drop than spam the system console with
    /// "no banner shown" errors.
    func emit(tabID: Int64, title: String, body: String) {
        guard authorization.authorized else { return }
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
    /// foreground: macOS otherwise suppresses banners when the
    /// originating app is frontmost.
    ///
    /// Under plan 002's policy B (§3.5) a notification for the tab you
    /// are looking at never reaches this class — the workspace drops it
    /// at arrival, along with the badge and the inbox row. So forcing
    /// foreground presentation here now means exactly one thing:
    /// deliver banners for *background* tabs while the Roost window is
    /// focused. That is the case this override exists for.
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
