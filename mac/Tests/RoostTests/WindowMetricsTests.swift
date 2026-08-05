// WindowMetricsTests — pins the `app.window_metrics` `terminal_top` /
// `terminal_font_family` derivation (issue #287, one-op-set parity: iced
// reports the chrome-band height + resolved family, GTK reports family only
// with `terminal_top: None`, and Mac was reporting neither before this).
//
// `RoostApp.terminalMetrics(contentView:terminalView:font:)` is the pure
// lever `UiBridge.terminalMetrics()` calls through — it takes plain `NSView`
// stand-ins rather than a live `RoostApp`/window (which isn't constructible
// headlessly; there's no existing test that builds one), so the flip-aware
// math and the nil-tolerance contract are both directly testable.
//
// XCTest, not swift-testing: a swarm of fast value-checks in the
// swift-testing suite reliably SIGABRTs `swiftpm-testing-helper` under
// Xcode 26.x (see `ShellEscapeTests.swift`'s header for the same note).

import AppKit
import Foundation
import XCTest

@testable import Roost

@MainActor
final class WindowMetricsTests: XCTestCase {
    /// Mirrors the real app's nesting (window.contentView → split → pane →
    /// terminalContainer → terminalView) closely enough to exercise
    /// `convert(_:to:)` across more than one hop, without needing the real
    /// private view types — a plain `NSView` stand-in is all the math
    /// touches.
    private func mountedTerminalStandIn(
        contentHeight: CGFloat,
        tabBarHeight: CGFloat,
        terminalHeight: CGFloat,
        terminalWidth: CGFloat = 800
    ) -> (contentView: NSView, terminalView: NSView) {
        let contentView = NSView(frame: NSRect(x: 0, y: 0, width: 1100, height: contentHeight))

        let pane = NSView(frame: contentView.bounds)
        contentView.addSubview(pane)

        // terminalContainer: pinned below a tabBarHeight-tall band at the
        // pane's top, flush to the pane's bottom — same shape as
        // `terminalContainer`'s constraints in App.swift.
        let container = NSView(
            frame: NSRect(x: 0, y: 0, width: pane.bounds.width, height: terminalHeight)
        )
        pane.addSubview(container)

        // terminalView fills its container exactly, matching the
        // edge-pin constraints `selectTab` applies in App.swift.
        let terminalView = NSView(frame: container.bounds)
        terminalView.frame.size.width = terminalWidth
        container.addSubview(terminalView)

        return (contentView, terminalView)
    }

    // MARK: - Positive derivation

    /// Pinned regression for the flip derivation: a tab bar band of 32pt
    /// (the Mac `tabBarHeight`) above a terminal filling the rest of a
    /// 700pt-tall content view must report `terminal_top == 32`, not the
    /// terminal view's raw (bottom-relative) `origin.y`.
    func testTerminalTopIsTheOffsetFromTheTopNotOriginY() {
        let tabBarHeight: CGFloat = 32
        let contentHeight: CGFloat = 700
        let (contentView, terminalView) = mountedTerminalStandIn(
            contentHeight: contentHeight,
            tabBarHeight: tabBarHeight,
            terminalHeight: contentHeight - tabBarHeight
        )

        // Sanity: the stand-in's origin.y is NOT the expected top offset —
        // proves the test would catch a regression back to raw frame math.
        XCTAssertEqual(terminalView.frame.origin.y, 0)

        let metrics = RoostApp.terminalMetrics(
            contentView: contentView,
            terminalView: terminalView,
            font: NSFont.systemFont(ofSize: 13)
        )
        XCTAssertEqual(metrics?.top, tabBarHeight)
    }

    /// A shorter/taller tab-bar band shifts the derived top by the same
    /// amount — pins that this is a live measurement, not a hardcoded
    /// constant.
    func testTerminalTopTracksAnArbitraryBandHeight() {
        let contentHeight: CGFloat = 500
        let bandHeight: CGFloat = 47
        let (contentView, terminalView) = mountedTerminalStandIn(
            contentHeight: contentHeight,
            tabBarHeight: bandHeight,
            terminalHeight: contentHeight - bandHeight
        )
        let metrics = RoostApp.terminalMetrics(
            contentView: contentView,
            terminalView: terminalView,
            font: NSFont.systemFont(ofSize: 13)
        )
        XCTAssertEqual(metrics?.top, bandHeight)
    }

    /// Font family resolution: `familyName` wins when present.
    func testFontFamilyPrefersFamilyNameOverFontName() {
        let (contentView, terminalView) = mountedTerminalStandIn(
            contentHeight: 700, tabBarHeight: 32, terminalHeight: 668
        )
        let font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let metrics = RoostApp.terminalMetrics(
            contentView: contentView, terminalView: terminalView, font: font
        )
        XCTAssertEqual(metrics?.fontFamily, font.familyName)
        XCTAssertFalse(metrics?.fontFamily.isEmpty ?? true)
    }

    // MARK: - Nil tolerance

    /// No active terminal view (fresh app, no tabs) → nils, not zeros or
    /// an empty-string family.
    func testNoTerminalViewYieldsNil() {
        let contentView = NSView(frame: NSRect(x: 0, y: 0, width: 1100, height: 700))
        let metrics = RoostApp.terminalMetrics(
            contentView: contentView,
            terminalView: nil,
            font: NSFont.systemFont(ofSize: 13)
        )
        XCTAssertNil(metrics)
    }

    /// No content view (no window yet) → nils.
    func testNoContentViewYieldsNil() {
        let terminalView = NSView(frame: NSRect(x: 0, y: 0, width: 800, height: 668))
        let container = NSView()
        container.addSubview(terminalView)
        let metrics = RoostApp.terminalMetrics(
            contentView: nil,
            terminalView: terminalView,
            font: NSFont.systemFont(ofSize: 13)
        )
        XCTAssertNil(metrics)
    }

    /// A terminal view that exists but isn't mounted (no superview) →
    /// nils, matching the "session exists but its view was just torn
    /// down/never attached" edge the guard exists for.
    func testUnmountedTerminalViewYieldsNil() {
        let contentView = NSView(frame: NSRect(x: 0, y: 0, width: 1100, height: 700))
        let terminalView = NSView(frame: NSRect(x: 0, y: 0, width: 800, height: 668))
        XCTAssertNil(terminalView.superview)
        let metrics = RoostApp.terminalMetrics(
            contentView: contentView,
            terminalView: terminalView,
            font: NSFont.systemFont(ofSize: 13)
        )
        XCTAssertNil(metrics)
    }

    /// No font (defensive — shouldn't happen in practice since
    /// `TerminalView.font` is non-optional) → nils rather than crashing.
    func testNoFontYieldsNil() {
        let contentView = NSView(frame: NSRect(x: 0, y: 0, width: 1100, height: 700))
        let terminalView = NSView(frame: NSRect(x: 0, y: 0, width: 800, height: 668))
        contentView.addSubview(terminalView)
        let metrics = RoostApp.terminalMetrics(
            contentView: contentView,
            terminalView: terminalView,
            font: nil
        )
        XCTAssertNil(metrics)
    }
}
