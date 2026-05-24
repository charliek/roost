// Pure-logic tests for the notification inbox store. Mirrors the
// repo's pattern of testing extracted state (PaletteStateTests,
// TabPillStateTests) without standing up AppKit. The GTK port
// (`notification_inbox.rs`) carries an equivalent set.

import Foundation
import Testing

@testable import Roost

private func rec(
    _ tabID: Int64,
    project: Int64 = 1,
    body: String = "needs your input",
    at: Date = Date()
) -> NotificationRecord {
    NotificationRecord(
        tabID: tabID,
        projectID: project,
        title: "proj · tab\(tabID)",
        body: body,
        at: at
    )
}

@Test
func upsertNewPrependsNewestFirst() {
    var inbox = NotificationInbox()
    inbox.upsert(rec(1))
    inbox.upsert(rec(2))
    inbox.upsert(rec(3))
    #expect(inbox.count == 3)
    // Newest first.
    #expect(inbox.snapshot().map(\.tabID) == [3, 2, 1])
}

@Test
func upsertExistingUpdatesInPlaceAndMovesToFront() {
    var inbox = NotificationInbox()
    inbox.upsert(rec(1, body: "first"))
    inbox.upsert(rec(2, body: "second"))
    // Re-fire on tab 1 → no duplicate, moves to front, body updated.
    inbox.upsert(rec(1, body: "updated"))
    #expect(inbox.count == 2)
    #expect(inbox.snapshot().map(\.tabID) == [1, 2])
    #expect(inbox.snapshot().first?.body == "updated")
}

@Test
func capsAtTenEvictingOldest() {
    var inbox = NotificationInbox()
    for i in 1...10 { inbox.upsert(rec(Int64(i))) }
    #expect(inbox.count == 10)
    // 11th distinct tab evicts the oldest (tab 1).
    inbox.upsert(rec(11))
    #expect(inbox.count == 10)
    let ids = inbox.snapshot().map(\.tabID)
    #expect(ids.first == 11)
    #expect(!ids.contains(1))
    #expect(ids.contains(2))
}

@Test
func removeDropsOnlyThatTab() {
    var inbox = NotificationInbox()
    inbox.upsert(rec(1))
    inbox.upsert(rec(2))
    inbox.remove(1)
    #expect(inbox.snapshot().map(\.tabID) == [2])
    // Removing an absent tab is a no-op.
    inbox.remove(99)
    #expect(inbox.count == 1)
}

@Test
func clearEmptiesEverything() {
    var inbox = NotificationInbox()
    inbox.upsert(rec(1))
    inbox.upsert(rec(2))
    inbox.clear()
    #expect(inbox.isEmpty)
    #expect(inbox.count == 0)
    #expect(inbox.tabIDs.isEmpty)
}

@Test
func tabIDsTracksPendingEntries() {
    var inbox = NotificationInbox()
    inbox.upsert(rec(5))
    inbox.upsert(rec(6))
    #expect(inbox.tabIDs == [6, 5])
}

@Test
func composeTitleIsProjectForward() {
    #expect(NotificationInbox.composeTitle(project: "roost", tab: "claude") == "roost · claude")
}

@Test
func relativeTimeBuckets() {
    let now = Date(timeIntervalSince1970: 1_000_000)
    #expect(relativeTimeLabel(from: now, now: now) == "just now")
    #expect(relativeTimeLabel(from: now.addingTimeInterval(-30), now: now) == "just now")
    #expect(relativeTimeLabel(from: now.addingTimeInterval(-120), now: now) == "2m")
    #expect(relativeTimeLabel(from: now.addingTimeInterval(-3600), now: now) == "1h")
    #expect(relativeTimeLabel(from: now.addingTimeInterval(-172800), now: now) == "2d")
    // Future timestamps clamp to "just now" rather than going negative.
    #expect(relativeTimeLabel(from: now.addingTimeInterval(60), now: now) == "just now")
}
