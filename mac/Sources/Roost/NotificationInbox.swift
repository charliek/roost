// In-memory inbox of pending agent notifications, surfaced through the
// command palette ("View Notifications") and the Dock badge.
//
// This is the PURE, AppKit-free store — like `PaletteState` /
// `TabPillState`, it's unit-tested in isolation. The UI wiring in
// App.swift composes records (from the live project/tab model) and
// drives membership off the `has_notification` edges it already
// receives; this file just keeps the ordered, deduped, capped list.
//
// Design: a LIVE inbox, not a history log. One entry per tab,
// newest-first, capped at `capacity`. The
// invariant is: a tab has a row here iff it has a pending notification
// (modulo cap eviction). Jumping to a tab — or clearing it — drops the
// row via the same false-edge that clears the sidebar dot.

import Foundation

/// One pending notification, keyed by `tabID` (the dedup key + jump
/// target). `title` is composed for display ("<project> · <tab>");
/// `body` is the latest message; `at` drives relative-time + ordering.
struct NotificationRecord: Equatable {
    let tabID: Int64
    let projectID: Int64
    var title: String
    var body: String
    var at: Date

    init(tabID: Int64, projectID: Int64, title: String, body: String, at: Date = Date()) {
        self.tabID = tabID
        self.projectID = projectID
        self.title = title
        self.body = body
        self.at = at
    }
}

/// Ordered, newest-first ring of pending notifications, one entry per
/// tab, capped at `capacity`. Pure value type — no UI dependencies.
struct NotificationInbox {
    /// Cap chosen to fit the palette card without scrolling; matches
    /// Roost's "tabs don't survive UI quits" ephemerality (no
    /// persistence, no history). With >cap pending tabs a tab can have
    /// a sidebar dot but no inbox row (evicted) — acceptable.
    static let capacity = 10

    private(set) var records: [NotificationRecord] = []

    /// Insert or update the entry for `record.tabID`. An existing tab's
    /// row is replaced (fresh body/title/time) and moved to the front;
    /// a new tab is prepended, evicting the oldest (tail) past capacity.
    mutating func upsert(_ record: NotificationRecord) {
        records.removeAll { $0.tabID == record.tabID }
        records.insert(record, at: 0)
        if records.count > Self.capacity {
            records.removeLast(records.count - Self.capacity)
        }
    }

    /// Drop the entry for `tabID` (focus / clear / session-end / close).
    /// No-op if absent.
    mutating func remove(_ tabID: Int64) {
        records.removeAll { $0.tabID == tabID }
    }

    /// Front-to-back (newest first) for rendering.
    func snapshot() -> [NotificationRecord] {
        records
    }

    var count: Int { records.count }
    var isEmpty: Bool { records.isEmpty }

    /// Tab ids of every pending entry — used by "Clear All" to clear
    /// each tab's notification through the normal false-edge.
    var tabIDs: [Int64] { records.map(\.tabID) }
}

// MARK: - Display helpers (pure, testable)

extension NotificationInbox {
    /// Compose the project-forward row title: "<project> · <tab>".
    static func composeTitle(project: String, tab: String) -> String {
        "\(project) · \(tab)"
    }
}

/// Compact relative-time label ("just now", "2m", "1h", "3d") for the
/// inbox row's trailing text. Mirrors the GTK `relative_time`.
func relativeTimeLabel(from date: Date, now: Date = Date()) -> String {
    let secs = max(0, Int(now.timeIntervalSince(date)))
    if secs < 60 { return "just now" }
    if secs < 3600 { return "\(secs / 60)m" }
    if secs < 86400 { return "\(secs / 3600)h" }
    return "\(secs / 86400)d"
}
