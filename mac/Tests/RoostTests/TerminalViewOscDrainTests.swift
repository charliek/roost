// End-to-end OSC drain tests that exercise the production
// `TerminalView.appendBytes` call site — not just the building blocks.
//
// `OscReplyTests.swift::osc{10,11,12}_dynamicSetIsReflectedByQueryReply`
// pins `liveColor(forQuery:)` and `formatColorQueryResponse` in
// isolation. They construct a bare libghostty terminal and call the
// helpers directly. That leaves the actual production wiring uncovered:
// if `appendBytes`'s drain stopped calling `liveColor(...)` (or read
// the stale theme, as the pre-#145 code did), those isolated tests
// would still pass.
//
// The cases below drive real OSC byte sequences through the same
// `TerminalView.appendBytes` the real `TabSession.outputDrainTask`
// calls (`TabSession.swift:175`, `:248`), with a stubbed `onKey`
// closure that captures the reply bytes. Order: SET in one
// `appendBytes` call (so libghostty processes it before the next
// `oscScanner.feed`), then QUERY in a second call (so the scanner
// emits `.colorQuery` against the post-set live color).
//
// Same-chunk SET+QUERY is a known #145 limitation and is NOT covered
// here — see PR C's `test_osc11_same_chunk_set_query_known_stale`
// regression slot.

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

@Test @MainActor
func appendBytes_osc11_dynamicSetReachesReplyViaDrain() {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())
    var captured: [Data] = []
    view.onKey = { captured.append($0) }

    // SET in its own call so libghostty's vt_write processes it
    // before the QUERY's scanner.feed runs.
    view.appendBytes(Data("\u{1B}]11;rgb:00/11/22\u{07}".utf8))
    view.appendBytes(Data("\u{1B}]11;?\u{07}".utf8))

    let replyText = captured.map { String(decoding: $0, as: UTF8.self) }.joined()
    #expect(
        replyText.contains("0000/1111/2222"),
        "reply must encode the post-set bg (got \(replyText))"
    )
    #expect(
        !replyText.contains("1c1c/1c1c/1c1c"),
        "reply must NOT encode the stale theme bg (got \(replyText))"
    )
}

@Test @MainActor
func appendBytes_osc10_dynamicSetReachesReplyViaDrain() {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())
    var captured: [Data] = []
    view.onKey = { captured.append($0) }

    view.appendBytes(Data("\u{1B}]10;rgb:aa/bb/cc\u{07}".utf8))
    view.appendBytes(Data("\u{1B}]10;?\u{07}".utf8))

    let replyText = captured.map { String(decoding: $0, as: UTF8.self) }.joined()
    #expect(replyText.contains("aaaa/bbbb/cccc"), "got \(replyText)")
    #expect(!replyText.contains("ffff/ffff/ffff"), "stale theme fg leaked: \(replyText)")
}

@Test @MainActor
func appendBytes_osc12_dynamicSetReachesReplyViaDrain() {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())
    var captured: [Data] = []
    view.onKey = { captured.append($0) }

    view.appendBytes(Data("\u{1B}]12;rgb:de/ad/be\u{07}".utf8))
    view.appendBytes(Data("\u{1B}]12;?\u{07}".utf8))

    let replyText = captured.map { String(decoding: $0, as: UTF8.self) }.joined()
    #expect(replyText.contains("dede/adad/bebe"), "got \(replyText)")
    #expect(!replyText.contains("9898/9898/9d9d"), "stale theme cursor leaked: \(replyText)")
}

/// Companion to the OSC-reply tests above: pins that the same
/// `appendBytes` event-fan-out routes non-reply OSCs through `onOsc`.
/// A refactor that broke the OSC reply path could easily break OSC
/// title / cwd / notification routing too if both share the drain;
/// this test catches that class of regression.
///
/// Note: the scanner decodes `file:///tmp` to `/tmp` via `parseOsc7`
/// (see `OscScanner.swift:325`), so the reported payload is the
/// already-decoded path rather than the raw `file://` URI.
@Test @MainActor
func appendBytes_routesOsc7CwdEventToOnOsc() {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())
    var captured: [(UInt32, String)] = []
    view.onOsc = { cmd, payload in captured.append((cmd, payload)) }

    view.appendBytes(Data("\u{1B}]7;file:///tmp\u{1B}\\".utf8))

    #expect(captured.count == 1, "expected exactly one OSC event (got \(captured.count))")
    #expect(captured.first?.0 == 7, "expected cmd 7 (got \(String(describing: captured.first?.0)))")
    #expect(
        captured.first?.1 == "/tmp",
        "expected decoded path /tmp (got \(String(describing: captured.first?.1)))"
    )
}
