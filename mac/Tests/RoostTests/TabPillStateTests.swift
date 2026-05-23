// Round-3 R5 tests for TabPillView's inline-rename state machine and
// `RoostApp.activeTabIndex` helper.
//
// These mirror `InlineRenameStateTests.swift`'s race-guard coverage on
// `ProjectRowCellView`: a sibling-driven `configure(...)` arriving
// while the user is mid-edit must not clobber the typing buffer. The
// helper test covers the pure index math `scrollActiveTabIntoView`
// relies on.

import AppKit
import Foundation
import Testing
@testable import Roost

@MainActor
@Test
func tabPillConfigureSkipsTitleWhileEditing() {
    let pill = TabPillView(
        index: 0,
        title: "Original",
        isActive: true,
        statusColor: nil,
        hasBadge: false,
        tabID: 7,
        onSelect: { _ in },
        onClose: { _ in },
        onRename: { _ in }
    )
    #expect(pill.editBufferTextForTesting == "Original")

    pill.beginEdit(
        initial: "Original",
        onCommit: { _, _ in },
        onCancel: { }
    )
    #expect(pill.isEditing == true)

    // Sibling-driven configure (rebuildTabBar firing for an unrelated
    // event arm — notification, cwd, state) must not clobber the
    // typing buffer mid-edit.
    pill.configure(
        index: 0,
        title: "fromCli",
        isActive: true,
        statusColor: nil,
        hasBadge: false,
        tabID: 7,
        onSelect: { _ in },
        onClose: { _ in },
        onRename: { _ in }
    )
    #expect(pill.editBufferTextForTesting == "Original")
    #expect(pill.isEditing == true)
}

@MainActor
@Test
func tabPillConfigureResyncsTitleAfterEdit() {
    let pill = TabPillView(
        index: 0,
        title: "Alpha",
        isActive: true,
        statusColor: nil,
        hasBadge: false,
        tabID: 1,
        onSelect: { _ in },
        onClose: { _ in },
        onRename: { _ in }
    )
    pill.beginEdit(
        initial: "Alpha",
        onCommit: { _, _ in },
        onCancel: { }
    )

    // Simulate focus-loss / Escape via `endEdit` (the public surface
    // for ending the edit without firing commit/cancel callbacks).
    pill.endEdit()
    #expect(pill.isEditing == false)

    pill.configure(
        index: 0,
        title: "Beta",
        isActive: true,
        statusColor: nil,
        hasBadge: false,
        tabID: 1,
        onSelect: { _ in },
        onClose: { _ in },
        onRename: { _ in }
    )
    #expect(pill.editBufferTextForTesting == "Beta")
}

@MainActor
@Test
func tabPillBeginEditCapturesInitialTitle() {
    // Round-3 R5: `onCommit` is invoked with both the user's typed
    // text AND the title at edit-start, so the commit handler's
    // no-op detection (round-2 CR-fix) compares against the actual
    // displayed text rather than `liveTitle`.
    let pill = TabPillView(
        index: 0,
        title: "first-title",
        isActive: true,
        statusColor: nil,
        hasBadge: false,
        tabID: 42,
        onSelect: { _ in },
        onClose: { _ in },
        onRename: { _ in }
    )
    var capturedCommitted: String?
    var capturedInitial: String?
    pill.beginEdit(
        initial: "the-prefilled-title",
        onCommit: { committed, initial in
            capturedCommitted = committed
            capturedInitial = initial
        },
        onCancel: { }
    )

    // Drive the Enter-key path through the public NSTextField delegate
    // surface. Cast through NSControl because the delegate signature
    // requires it; the pill's own `label` field stands in.
    pill.controlTextDidEndEditing(
        Notification(name: NSControl.textDidEndEditingNotification, object: nil)
    )
    // textDidEndEditing routes to cancel — re-do via explicit
    // beginEdit and the doCommandBy path.
    #expect(capturedInitial == nil)  // cancel doesn't fire commit

    pill.beginEdit(
        initial: "the-prefilled-title",
        onCommit: { committed, initial in
            capturedCommitted = committed
            capturedInitial = initial
        },
        onCancel: { }
    )
    // The label's stringValue is what gets committed — explicitly set
    // it to simulate the user typing.
    // We can't directly set label.stringValue (private), but
    // editBufferTextForTesting reads it. Use the public delegate path
    // — the NSResponder.insertNewline(_:) selector triggers commit.
    // For the harness we need an NSTextView; using a stub.
    let stubText = NSTextField()
    let stubView = NSTextView()
    _ = pill.control(
        stubText,
        textView: stubView,
        doCommandBy: #selector(NSResponder.insertNewline(_:))
    )
    #expect(capturedInitial == "the-prefilled-title")
    #expect(capturedCommitted == "the-prefilled-title")
}

@MainActor
@Test
func activeTabIndexFindsOffscreenTab() {
    // Round-3 R2: extracted helper for the pill-index math used by
    // `scrollActiveTabIntoView`. Doesn't touch AppKit at all — just
    // identity comparison.
    let theme = Theme.fallback
    let a = TabSession(projectID: 1, theme: theme)
    let b = TabSession(projectID: 1, theme: theme)
    let c = TabSession(projectID: 1, theme: theme)
    let tabs = [a, b, c]

    #expect(RoostApp.activeTabIndex(tabs: tabs, active: a) == 0)
    #expect(RoostApp.activeTabIndex(tabs: tabs, active: b) == 1)
    #expect(RoostApp.activeTabIndex(tabs: tabs, active: c) == 2)
    #expect(RoostApp.activeTabIndex(tabs: tabs, active: nil) == nil)

    let stranger = TabSession(projectID: 99, theme: theme)
    #expect(RoostApp.activeTabIndex(tabs: tabs, active: stranger) == nil)
}
