// Headless tests for the write_pty device-query reply path (#247),
// exercised through the production `TerminalView.appendBytes` /
// resize call sites — not the roost-vt building blocks in isolation.
//
// `crates/roost-vt/tests/write_pty_test.rs` pins the engine's replies +
// the buffer API. These tests pin the *Mac wiring*: that
// `TerminalView` installs the callback, drains it into `onKey`
// synchronously inside the producing `appendBytes` (collect-then-send),
// drains again after a resize (mode 2048 in-band size report, which
// fires outside vt_write), and retains replies across a nil→installed
// `onKey` (attach-race parity with the Linux drain).
//
// Model: `TerminalViewOscDrainTests.swift` — construct the view, stub
// `onKey`, feed real byte sequences through `appendBytes`.

import AppKit
import CGhosttyVT
import Testing

@testable import Roost

private func testTheme() -> Theme {
    Theme(
        foreground: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        background: NSColor(srgbRed: 0x1c / 255.0, green: 0x1c / 255.0, blue: 0x1c / 255.0, alpha: 1),
        cursor: NSColor(srgbRed: 0x98 / 255.0, green: 0x98 / 255.0, blue: 0x9d / 255.0, alpha: 1),
        selectionBackground: .gray,
        selectionForeground: .white,
        palette: Array(repeating: .gray, count: 256)
    )
}

/// DA1 (`ESC[c`) — the primary device-attributes query crossterm blocks
/// on. The engine's default reply must be delivered via `onKey` within
/// the same `appendBytes` call that fed the query (collect-then-send).
@Test @MainActor
func appendBytes_da1_repliesSynchronously() {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())
    var captured: [Data] = []
    view.onKey = { captured.append($0) }

    view.appendBytes(Data("\u{1B}[c".utf8))

    // Delivered by the time appendBytes returns — no second call needed.
    let reply = captured.map { String(decoding: $0, as: UTF8.self) }.joined()
    #expect(reply == "\u{1B}[?62;22c", "DA1 reply (got \(reply.debugDescription))")
}

/// DSR 6n cursor-position report on a fresh terminal → row 1, col 1.
@Test @MainActor
func appendBytes_dsr6n_cursorPositionReport() {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())
    var captured: [Data] = []
    view.onKey = { captured.append($0) }

    view.appendBytes(Data("\u{1B}[6n".utf8))

    let reply = captured.map { String(decoding: $0, as: UTF8.self) }.joined()
    #expect(reply == "\u{1B}[1;1R", "CPR reply (got \(reply.debugDescription))")
}

/// DECRQM for mode 7 (wraparound, set by default) → state 1 (set).
@Test @MainActor
func appendBytes_decrqm_wraparoundIsSet() {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())
    var captured: [Data] = []
    view.onKey = { captured.append($0) }

    view.appendBytes(Data("\u{1B}[?7$p".utf8))

    let reply = captured.map { String(decoding: $0, as: UTF8.self) }.joined()
    #expect(reply == "\u{1B}[?7;1$y", "DECRQM reply (got \(reply.debugDescription))")
}

/// Kitty keyboard progressive-enhancement query (`ESC[?u`) with no
/// flags pushed → `ESC[?0u`. This is the #247 reply crossterm's
/// `supports_keyboard_enhancement()` waits on.
@Test @MainActor
func appendBytes_kittyKeyboardQuery_reportsZeroFlags() {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())
    var captured: [Data] = []
    view.onKey = { captured.append($0) }

    view.appendBytes(Data("\u{1B}[?u".utf8))

    let reply = captured.map { String(decoding: $0, as: UTF8.self) }.joined()
    #expect(reply == "\u{1B}[?0u", "Kitty query reply (got \(reply.debugDescription))")
}

/// Mode 2048 (in-band size reports) fires the write_pty callback
/// synchronously inside `ghostty_terminal_resize`, *outside* vt_write.
/// The reply must be drained by the post-resize flush — reached here by
/// forcing a grid reflow via `setFrameSize`, with NO further
/// `appendBytes`.
@Test @MainActor
func resize_mode2048_emitsInBandSizeReport() {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())
    var captured: [Data] = []
    view.onKey = { captured.append($0) }

    // Enabling mode 2048 emits an initial in-band report through the
    // vt_write drain; discard it so the assertion covers only the resize.
    view.appendBytes(Data("\u{1B}[?2048h".utf8))
    captured.removeAll()

    // Shrink to a grid that fits 40x12 cells → dims change → resize
    // fires. No appendBytes between here and the assertion.
    let cell = view.cellSize
    view.setFrameSize(NSSize(width: cell.width * 40, height: cell.height * 12))

    let report = captured.map { String(decoding: $0, as: UTF8.self) }.joined()
    // CSI 48 ; rows ; cols ; height_px ; width_px t
    #expect(
        report.range(of: "^\u{1B}\\[48;12;40;[0-9]+;[0-9]+t$", options: .regularExpression) != nil,
        "expected an in-band size report for 12x40 (got \(report.debugDescription))"
    )
}

/// Attach-race parity with the Linux drain: a reply produced while
/// `onKey` is nil stays buffered, and is delivered on the next drain
/// once `onKey` is installed.
@Test @MainActor
func nilOnKey_retainsReplyUntilInstalled() {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())

    // onKey is nil — the DA1 reply must be buffered, not dropped.
    view.appendBytes(Data("\u{1B}[c".utf8))

    var captured: [Data] = []
    view.onKey = { captured.append($0) }

    // Feed a plain printable byte (no reply of its own) — it still runs
    // the drain, which now has a sink and delivers the retained reply.
    view.appendBytes(Data("x".utf8))

    let reply = captured.map { String(decoding: $0, as: UTF8.self) }.joined()
    #expect(reply == "\u{1B}[?62;22c", "retained DA1 reply (got \(reply.debugDescription))")
}
