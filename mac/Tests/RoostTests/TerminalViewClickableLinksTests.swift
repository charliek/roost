// Production-path tests for Cmd-click URL launching on Mac.
//
// These cases drive the same `handleLinkClick` the real `mouseDown`
// dispatches to — extracted from `mouseDown(with:)` so test cases can
// exercise the click path without fabricating an `NSEvent` (which
// `NSEvent.init` makes unduly painful in swift-testing). The
// production code goes through the same helper, so coverage holds.
//
// `CapturingUrlLauncher` (`mac/Sources/Roost/UrlLauncher.swift`)
// substitutes for `NSWorkspace.shared.open` — the tests assert against
// `capturing.opened` and don't launch a real browser.

import AppKit
import CGhosttyVT
import Testing

@testable import Roost

private func clickableTestTheme() -> Theme {
    Theme(
        foreground: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        background: NSColor(srgbRed: 0, green: 0, blue: 0, alpha: 1),
        cursor: NSColor(srgbRed: 0.5, green: 0.5, blue: 0.5, alpha: 1),
        selectionBackground: .gray,
        selectionForeground: .white,
        palette: Array(repeating: .gray, count: 256)
    )
}

@Test @MainActor
func cmdClick_onPlainURL_opensWorkspace() {
    let view = TerminalView(cols: 80, rows: 24, theme: clickableTestTheme())
    let launcher = CapturingUrlLauncher()
    view.urlLauncher = launcher

    // Feed PTY output containing a plain regex-detectable URL. Row 0,
    // cols 6..24 will hold `https://example.com`.
    view.appendBytes(Data("Visit https://example.com today".utf8))

    // Cmd-click on column 10 — inside the URL span.
    let consumed = view.handleLinkClick(col: 10, row: 0, commandHeld: true)
    #expect(consumed, "Cmd-click on URL must consume the click")
    #expect(launcher.opened == [URL(string: "https://example.com")!])
}

@Test @MainActor
func cmdClick_onOsc8Hyperlink_opensTheURI() {
    let view = TerminalView(cols: 80, rows: 24, theme: clickableTestTheme())
    let launcher = CapturingUrlLauncher()
    view.urlLauncher = launcher

    // OSC 8: wraps `here` with `https://real.example/path`. The plain
    // text `here` does not match the URL regex; the OSC 8 binding
    // wins.
    view.appendBytes(Data("\u{1B}]8;;https://real.example/path\u{1B}\\here\u{1B}]8;;\u{1B}\\".utf8))

    let consumed = view.handleLinkClick(col: 1, row: 0, commandHeld: true)
    #expect(consumed, "Cmd-click on OSC 8 cell must consume the click")
    #expect(launcher.opened == [URL(string: "https://real.example/path")!])
}

@Test @MainActor
func cmdClick_outsideURL_doesNotOpen() {
    let view = TerminalView(cols: 80, rows: 24, theme: clickableTestTheme())
    let launcher = CapturingUrlLauncher()
    view.urlLauncher = launcher

    view.appendBytes(Data("Visit https://example.com today".utf8))

    // Column 30 is past the URL — should fall through to selection.
    let consumed = view.handleLinkClick(col: 30, row: 0, commandHeld: true)
    #expect(!consumed, "Cmd-click off URL must NOT consume the click")
    #expect(launcher.opened.isEmpty, "no URL opened: \(launcher.opened)")
}

@Test @MainActor
func plainClick_doesNotOpenURL() {
    let view = TerminalView(cols: 80, rows: 24, theme: clickableTestTheme())
    let launcher = CapturingUrlLauncher()
    view.urlLauncher = launcher

    view.appendBytes(Data("Visit https://example.com today".utf8))

    // Click on the URL but WITHOUT Cmd — must fall through.
    let consumed = view.handleLinkClick(col: 10, row: 0, commandHeld: false)
    #expect(!consumed, "Plain click on URL must NOT consume the click")
    #expect(launcher.opened.isEmpty, "no URL opened: \(launcher.opened)")
}

@Test @MainActor
func cmdClick_osc8WinsOverRegex() {
    // A row with both: an OSC 8 wrapper around text that ALSO regex-
    // matches as a URL. The OSC 8 URI must win.
    let view = TerminalView(cols: 80, rows: 24, theme: clickableTestTheme())
    let launcher = CapturingUrlLauncher()
    view.urlLauncher = launcher

    // OSC 8 wraps `https://link.example` with the URI `https://real.example`.
    // Without OSC 8 priority, the regex would match the visible
    // `https://link.example` text and open that instead.
    let osc = "\u{1B}]8;;https://real.example\u{1B}\\https://link.example\u{1B}]8;;\u{1B}\\"
    view.appendBytes(Data(osc.utf8))

    let consumed = view.handleLinkClick(col: 5, row: 0, commandHeld: true)
    #expect(consumed)
    #expect(launcher.opened == [URL(string: "https://real.example")!])
}
