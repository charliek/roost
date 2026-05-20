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
func dragExitedWithoutActiveDragDoesNotFireOnDragEnded() {
    // CR on PR #75: `TabBarScrollView` forwards drag-exit /
    // conclude events indiscriminately, including non-tab drags
    // that never installed our placeholder. The teardown path must
    // be a no-op (no `onDragEnded`, no rebuild) when there was no
    // tab drag in flight.
    let stack = TabBarStackView()
    var endedCount = 0
    stack.onDragEnded = { endedCount += 1 }

    stack.draggingExited(nil)

    #expect(stack.isDragInProgress == false)
    #expect(endedCount == 0)
}

@MainActor
@Test
func concludeDragWithoutActiveDragDoesNotFireOnDragEnded() {
    let stack = TabBarStackView()
    var endedCount = 0
    stack.onDragEnded = { endedCount += 1 }

    stack.concludeDragOperation(nil)

    #expect(stack.isDragInProgress == false)
    #expect(endedCount == 0)
}

@MainActor
@Test
func rebuildPendingFlagAlwaysFlushesEvenWithoutActiveDrag() {
    // The App's `rebuildTabBar` sets `rebuildPending = true` when
    // it early-returns during a drag. The pending-rebuild branch
    // must drain regardless of whether `hadActiveDrag` is true —
    // otherwise a deferred rebuild could be lost if the drag state
    // already cleared by another code path.
    let stack = TabBarStackView()
    stack.rebuildPending = true

    var endedCount = 0
    stack.onDragEnded = { endedCount += 1 }

    stack.concludeDragOperation(nil)

    #expect(stack.rebuildPending == false)
    #expect(endedCount == 1)
}
