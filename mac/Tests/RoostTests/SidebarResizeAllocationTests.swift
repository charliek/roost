// SidebarResizeAllocationTests — the resize math that fixes the
// sidebar-grows-on-window-resize bug.
//
// `computeSidebarResizeAllocation` is the pure-function lever the
// `NSSplitViewDelegate.splitView(_:resizeSubviewsWithOldSize:)`
// callback dispatches through. The runtime path (App.swift) just
// reads the input frames off NSSplitView and writes the output frames
// back; the decision logic lives here.
//
// Coverage targets:
//   - Window grew → sidebar holds, content absorbs the delta.
//   - Window shrank → same.
//   - Width unchanged (= user drag) → `.useDefault`, let
//     NSSplitView's normal adjustSubviews do its thing.
//   - Sub-pixel float diff (rounding noise) → treat as no change.
//   - Sidebar at width 0 (⌘B-collapsed) → `.useDefault` to honor
//     the collapsed state.
//   - Out-of-band stored width → clamped to [min, max].

import Foundation
import Testing

@testable import Roost

@Suite("Sidebar resize allocation")
struct SidebarResizeAllocationTests {
    private let minWidth: CGFloat = 160
    private let maxWidth: CGFloat = 400
    private let divider: CGFloat = 1

    private func currentFrame(width: CGFloat, height: CGFloat = 700) -> NSRect {
        NSRect(x: 0, y: 0, width: width, height: height)
    }

    /// 1100 → 1800 window grow: sidebar holds at its 220pt seat,
    /// content gets the full 700pt delta. This is the regression
    /// case for the bug PR #159 misdiagnosed.
    @Test func windowGrowSidebarHoldsContentAbsorbs() {
        let alloc = computeSidebarResizeAllocation(
            splitViewSize: NSSize(width: 1800, height: 700),
            oldSize: NSSize(width: 1100, height: 700),
            currentSidebarFrame: currentFrame(width: 220),
            dividerThickness: divider,
            minWidth: minWidth,
            maxWidth: maxWidth
        )
        #expect(alloc.sidebar?.width == 220, "sidebar must hold its current width")
        #expect(alloc.content?.width == 1800 - 220 - divider,
                "content absorbs the entire delta")
        #expect(alloc.sidebar?.origin.x == 0)
        #expect(alloc.content?.origin.x == 220 + divider,
                "content starts after sidebar + divider")
    }

    /// 1800 → 1100 window shrink: symmetric — sidebar holds, content
    /// gives back the 700pt.
    @Test func windowShrinkSidebarHoldsContentGivesBack() {
        let alloc = computeSidebarResizeAllocation(
            splitViewSize: NSSize(width: 1100, height: 700),
            oldSize: NSSize(width: 1800, height: 700),
            currentSidebarFrame: currentFrame(width: 220),
            dividerThickness: divider,
            minWidth: minWidth,
            maxWidth: maxWidth
        )
        #expect(alloc.sidebar?.width == 220)
        #expect(alloc.content?.width == 1100 - 220 - divider)
    }

    /// No width change → user drag (or a no-op layout pass). Defer
    /// to NSSplitView's adjustSubviews; the drag has already moved
    /// the divider and we shouldn't fight it.
    @Test func userDragYieldsDefaultPath() {
        let alloc = computeSidebarResizeAllocation(
            splitViewSize: NSSize(width: 1100, height: 700),
            oldSize: NSSize(width: 1100, height: 700),
            currentSidebarFrame: currentFrame(width: 280),
            dividerThickness: divider,
            minWidth: minWidth,
            maxWidth: maxWidth
        )
        #expect(alloc == .useDefault, "user drag must use NSSplitView's default")
    }

    /// Sub-pixel float diff (under the 0.5pt tolerance) is treated
    /// as "no change" — AppKit can deliver these on layout passes
    /// triggered by tab-bar height changes etc. without actually
    /// resizing the window.
    @Test func subPixelDiffIsTreatedAsNoChange() {
        let alloc = computeSidebarResizeAllocation(
            splitViewSize: NSSize(width: 1100.3, height: 700),
            oldSize: NSSize(width: 1100, height: 700),
            currentSidebarFrame: currentFrame(width: 220),
            dividerThickness: divider,
            minWidth: minWidth,
            maxWidth: maxWidth
        )
        #expect(alloc == .useDefault)
    }

    /// ⌘B-collapsed: sidebar.frame.width == 0. Don't hand the
    /// collapsed sidebar any width back on window resize; let
    /// adjustSubviews honor the collapse.
    @Test func collapsedSidebarYieldsDefaultPath() {
        let alloc = computeSidebarResizeAllocation(
            splitViewSize: NSSize(width: 1800, height: 700),
            oldSize: NSSize(width: 1100, height: 700),
            currentSidebarFrame: currentFrame(width: 0),
            dividerThickness: divider,
            minWidth: minWidth,
            maxWidth: maxWidth
        )
        #expect(alloc == .useDefault, "collapsed pane must stay collapsed")
    }

    /// Stored sidebar width above the configured max — clamp on
    /// output so a previously-saved out-of-band value can't widen
    /// the sidebar past 400 here.
    @Test func sidebarFrameAboveMaxIsClampedDown() {
        let alloc = computeSidebarResizeAllocation(
            splitViewSize: NSSize(width: 1800, height: 700),
            oldSize: NSSize(width: 1100, height: 700),
            currentSidebarFrame: currentFrame(width: 500),
            dividerThickness: divider,
            minWidth: minWidth,
            maxWidth: maxWidth
        )
        #expect(alloc.sidebar?.width == maxWidth, "above-max clamped to max")
        #expect(alloc.content?.width == 1800 - maxWidth - divider)
    }

    /// Stored sidebar width below the configured min — clamp up.
    @Test func sidebarFrameBelowMinIsClampedUp() {
        let alloc = computeSidebarResizeAllocation(
            splitViewSize: NSSize(width: 1800, height: 700),
            oldSize: NSSize(width: 1100, height: 700),
            currentSidebarFrame: currentFrame(width: 100),
            dividerThickness: divider,
            minWidth: minWidth,
            maxWidth: maxWidth
        )
        #expect(alloc.sidebar?.width == minWidth, "below-min clamped to min")
    }

    /// Window narrower than sidebar + divider — content width
    /// clamps to 0 rather than going negative. Defensive: real
    /// AppKit min-size constraint should prevent this, but the
    /// math should still be sane.
    @Test func contentNeverGoesNegative() {
        let alloc = computeSidebarResizeAllocation(
            splitViewSize: NSSize(width: 100, height: 700),
            oldSize: NSSize(width: 200, height: 700),
            currentSidebarFrame: currentFrame(width: 220),
            dividerThickness: divider,
            minWidth: minWidth,
            maxWidth: maxWidth
        )
        #expect(alloc.content?.width == 0, "content clamps to 0, not negative")
    }
}
