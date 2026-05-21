// Round-7 R7.D tests for the tab-strip drop indicator.
//
// R6.C's live-shift placeholder was replaced with a thin vertical
// accent bar shown at the would-land slot during a drag. The state
// machine is much simpler than the placeholder's: indicator NSView
// either exists or it doesn't, and its X position is a pure function
// of the cursor X and the current pill frames.
//
// These tests cover both halves: lifecycle (install + tear-down via
// the public hooks `draggingExited` and `concludeDragOperation`),
// and position math (`dropIndicatorX(forCursorX:pillFrames:
// indicatorWidth:)` — the pure static form that takes pill frames
// directly so we don't depend on NSStackView's layout pass).

import AppKit
import Foundation
import Testing
@testable import Roost

@MainActor
@Test
func indicatorStartsInactive() {
    let stack = TabBarStackView()
    #expect(stack.isDropIndicatorActive == false)
}

@MainActor
@Test
func showIndicatorActivatesIndicator() {
    let stack = TabBarStackView()
    stack.showIndicator(forCursorX: 0)
    #expect(stack.isDropIndicatorActive == true)
}

@MainActor
@Test
func removeIndicatorClearsActiveIndicator() {
    let stack = TabBarStackView()
    stack.showIndicator(forCursorX: 0)
    #expect(stack.isDropIndicatorActive == true)

    stack.removeIndicator()
    #expect(stack.isDropIndicatorActive == false)
}

@MainActor
@Test
func removeIndicatorIsIdempotent() {
    let stack = TabBarStackView()
    stack.removeIndicator()
    stack.removeIndicator()
    #expect(stack.isDropIndicatorActive == false)
}

@MainActor
@Test
func draggingExitedClearsIndicator() {
    let stack = TabBarStackView()
    stack.showIndicator(forCursorX: 50)
    #expect(stack.isDropIndicatorActive == true)

    stack.draggingExited(nil)
    #expect(stack.isDropIndicatorActive == false)
}

@MainActor
@Test
func concludeDragOperationClearsIndicator() {
    let stack = TabBarStackView()
    stack.showIndicator(forCursorX: 50)
    #expect(stack.isDropIndicatorActive == true)

    stack.concludeDragOperation(nil)
    #expect(stack.isDropIndicatorActive == false)
}

// MARK: - Position math

/// Pill frames mirroring a 3-pill strip: pill widths 80pt, 6pt gaps,
/// starting at x = 0. Stack-relative coords.
private let threePillFrames: [CGRect] = [
    CGRect(x: 0, y: 4, width: 80, height: 24), // midX = 40, maxX = 80
    CGRect(x: 86, y: 4, width: 80, height: 24), // midX = 126, maxX = 166
    CGRect(x: 172, y: 4, width: 80, height: 24), // midX = 212, maxX = 252
]

@MainActor
@Test
func cursorLeftOfPill0MidLandsAtLeadingEdge() {
    // Cursor in the left half of pill 0 (x < 40) → slot 0. The
    // indicator is flush against pill 0's leading edge — clamped
    // to x=0 (rather than `pill.frame.minX - indicatorWidth = -3`)
    // so the scroll view's clip view doesn't crop the leading
    // 3pt off the indicator when pill 0 sits flush at x=0.
    let x = TabBarStackView.dropIndicatorX(
        forCursorX: 20,
        pillFrames: threePillFrames,
        indicatorWidth: TabBarStackView.indicatorWidth
    )
    #expect(x == 0)
}

@MainActor
@Test
func cursorBetweenPills0And1LandsInGapCenter() {
    // Cursor in the right half of pill 0 (40 < x < 80) → slot 1.
    // Gap center is at (80 + 86) / 2 = 83; indicator left edge is
    // 83 - 1.5 = 81.5.
    let x = TabBarStackView.dropIndicatorX(
        forCursorX: 70,
        pillFrames: threePillFrames,
        indicatorWidth: TabBarStackView.indicatorWidth
    )
    let expected: CGFloat = (80 + 86) / 2 - TabBarStackView.indicatorWidth / 2
    #expect(x == expected)
}

@MainActor
@Test
func cursorBetweenPills1And2LandsInGapCenter() {
    // Cursor in the right half of pill 1 (126 < x < 166) → slot 2.
    // Gap center is at (166 + 172) / 2 = 169; indicator left edge
    // is 169 - 1.5 = 167.5.
    let x = TabBarStackView.dropIndicatorX(
        forCursorX: 150,
        pillFrames: threePillFrames,
        indicatorWidth: TabBarStackView.indicatorWidth
    )
    let expected: CGFloat = (166 + 172) / 2 - TabBarStackView.indicatorWidth / 2
    #expect(x == expected)
}

@MainActor
@Test
func cursorPastLastMidLandsAtTrailingEdge() {
    // Cursor past pill 2's midX (x ≥ 212) → slot 3, indicator's
    // left edge is at `last.maxX + indicatorWidth = 252 + 3 = 255`.
    let x = TabBarStackView.dropIndicatorX(
        forCursorX: 500,
        pillFrames: threePillFrames,
        indicatorWidth: TabBarStackView.indicatorWidth
    )
    #expect(x == 252 + TabBarStackView.indicatorWidth)
}

@MainActor
@Test
func emptyStackReturnsZero() {
    let x = TabBarStackView.dropIndicatorX(
        forCursorX: 100,
        pillFrames: [],
        indicatorWidth: TabBarStackView.indicatorWidth
    )
    #expect(x == 0)
}

@MainActor
@Test
func singlePillLeftOfMidLandsAtLeadingEdge() {
    // Pill 0 inset 10pt from the leading edge: 10 - 3 = 7, well
    // above the 0-clamp, so the indicator lands at x=7.
    let pill = CGRect(x: 10, y: 4, width: 80, height: 24) // midX = 50
    let x = TabBarStackView.dropIndicatorX(
        forCursorX: 30,
        pillFrames: [pill],
        indicatorWidth: TabBarStackView.indicatorWidth
    )
    #expect(x == 10 - TabBarStackView.indicatorWidth)
}

@MainActor
@Test
func singlePillRightOfMidLandsAtTrailingEdge() {
    let pill = CGRect(x: 10, y: 4, width: 80, height: 24) // midX = 50, maxX = 90
    let x = TabBarStackView.dropIndicatorX(
        forCursorX: 70,
        pillFrames: [pill],
        indicatorWidth: TabBarStackView.indicatorWidth
    )
    #expect(x == 90 + TabBarStackView.indicatorWidth)
}
