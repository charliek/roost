// Round-6 R6.C tests for the Ghostty-style drop placeholder.
//
// The placeholder lifecycle is a synchronous state machine inside
// `TabBarStackView`. The tricky surface is the hit-test math, which
// needs to skip the hidden source pill and include the placeholder
// — without that, the daemon's drop slot would diverge from what
// the user sees on screen.
//
// These tests poke the public surface (`isDragInProgress` flag,
// `rebuildPending` flag, `onDragEnded` callback) and indirectly
// verify the placeholder lifecycle via the same callbacks the App
// observes in production.

import AppKit
import Foundation
import Testing
@testable import Roost

@MainActor
@Test
func dragInProgressFlagStartsFalse() {
    let stack = TabBarStackView()
    #expect(stack.isDragInProgress == false)
    #expect(stack.rebuildPending == false)
}

@MainActor
@Test
func onDragEndedFiresAfterDragExited() {
    // Simulate the drag-cancel path: `draggingExited` is called when
    // the cursor leaves the strip's bounds without a drop. The flag
    // should reset to false and `onDragEnded` should fire so the App
    // can flush a pending rebuild.
    let stack = TabBarStackView()
    var endedCount = 0
    stack.onDragEnded = { endedCount += 1 }

    // `draggingExited` is the public AppKit entry point we want to
    // verify; it routes to `teardownPlaceholder(commit: false)`
    // which clears state and fires the callback even if no
    // placeholder was installed (idempotent teardown).
    stack.draggingExited(nil)

    #expect(stack.isDragInProgress == false)
    #expect(endedCount == 1)
}

@MainActor
@Test
func onDragEndedFiresAfterConcludeDragOperation() {
    let stack = TabBarStackView()
    var endedCount = 0
    stack.onDragEnded = { endedCount += 1 }

    stack.concludeDragOperation(nil)

    #expect(stack.isDragInProgress == false)
    #expect(endedCount == 1)
}

@MainActor
@Test
func rebuildPendingFlagFlipsExternallyAndResetsOnDragEnd() {
    // The App's `rebuildTabBar` sets `rebuildPending = true` when
    // it early-returns during a drag. `teardownPlaceholder` should
    // reset it back to false so the next rebuild starts clean.
    let stack = TabBarStackView()
    stack.rebuildPending = true

    var endedCount = 0
    stack.onDragEnded = { endedCount += 1 }

    stack.concludeDragOperation(nil)

    #expect(stack.rebuildPending == false)
    #expect(endedCount == 1)
}
