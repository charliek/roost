// End-to-end OSC drain tests that exercise the production
// `TerminalView.appendBytes` call site — not just the building blocks.
//
// Roost used to synthesise the OSC 4 / 10 / 11 / 12 query replies here.
// It no longer does: libghostty answers them itself through the
// `write_pty` effect as of the pinned ghostty `f2d5758f6` (upstream
// `14c829883`), and `appendBytes` drains that buffer into `onKey` after
// `vt_write`. Answering on both sides put a SECOND reply on the wire.
//
// So these cases pin the whole production chain — theme push through
// `Theme.apply`, `oscScanner.feed` passing the query through,
// `vt_write`, `flushPendingPtyReplies`, `onKey` — and assert the reply
// bytes EXACTLY, plus that there is exactly one of them. A regression
// that re-adds a Roost-side reply fails on the count; one that drops
// the `write_pty` wiring fails on the absence.
//
// The Swift↔Rust parity mirror for the reply bytes now lives at the
// engine level: `crates/roost-vt/tests/write_pty_test.rs` asserts the
// same sequences through the same libghostty.

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

/// Feed chunks through the production `appendBytes` and return every
/// byte the view handed to `onKey` — the tab's PTY-input sink.
@MainActor
private func repliesFor(_ chunks: [String]) -> String {
    let view = TerminalView(cols: 80, rows: 24, theme: testTheme())
    var captured: [Data] = []
    view.onKey = { captured.append($0) }
    for chunk in chunks {
        view.appendBytes(Data(chunk.utf8))
    }
    return captured.map { String(decoding: $0, as: UTF8.self) }.joined()
}

@Test @MainActor
func appendBytes_osc11QueryIsAnsweredExactlyOnceFromTheTheme() {
    // The theme `TerminalView.init` pushed through `Theme.apply` is
    // what libghostty reports: bg #1c1c1c in the 16-bit form.
    #expect(repliesFor(["\u{1B}]11;?\u{07}"]) == "\u{1B}]11;rgb:1c1c/1c1c/1c1c\u{07}")
}

@Test @MainActor
func appendBytes_osc10And12QueriesAreAnsweredFromTheTheme() {
    #expect(repliesFor(["\u{1B}]10;?\u{07}"]) == "\u{1B}]10;rgb:ffff/ffff/ffff\u{07}")
    #expect(repliesFor(["\u{1B}]12;?\u{07}"]) == "\u{1B}]12;rgb:9898/9898/9d9d\u{07}")
}

@Test @MainActor
func appendBytes_osc11_dynamicSetReachesReplyViaDrain() {
    // SET in one call, QUERY in the next: the reply must carry the
    // post-set bg, never the theme's.
    let replies = repliesFor(["\u{1B}]11;rgb:00/11/22\u{07}", "\u{1B}]11;?\u{07}"])
    #expect(replies == "\u{1B}]11;rgb:0000/1111/2222\u{07}", "got \(replies)")
}

@Test @MainActor
func appendBytes_osc10_dynamicSetReachesReplyViaDrain() {
    let replies = repliesFor(["\u{1B}]10;rgb:aa/bb/cc\u{07}", "\u{1B}]10;?\u{07}"])
    #expect(replies == "\u{1B}]10;rgb:aaaa/bbbb/cccc\u{07}", "got \(replies)")
}

@Test @MainActor
func appendBytes_osc12_dynamicSetReachesReplyViaDrain() {
    let replies = repliesFor(["\u{1B}]12;rgb:de/ad/be\u{07}", "\u{1B}]12;?\u{07}"])
    #expect(replies == "\u{1B}]12;rgb:dede/adad/bebe\u{07}", "got \(replies)")
}

/// Sequential semantics: a SET and a QUERY in ONE chunk answer with the
/// just-set color, once. Roost's own scanner used to answer such a
/// query from the pre-chunk color, which is both the wrong value and
/// the second answer.
@Test @MainActor
func appendBytes_sameChunkSetAndQueryReplyOnceSequentially() {
    let replies = repliesFor(["\u{1B}]11;rgb:00/11/22\u{07}\u{1B}]11;?\u{07}"])
    #expect(replies == "\u{1B}]11;rgb:0000/1111/2222\u{07}", "got \(replies)")
}

/// OSC 4: the probe opencode/opentui gate all their color detection on
/// (`OSC 4;0;?`, 300 ms timeout), and a mid-session palette set.
@Test @MainActor
func appendBytes_osc4QueryIsAnsweredViaDrain() {
    let replies = repliesFor(["\u{1B}]4;5;rgb:de/ad/be\u{07}", "\u{1B}]4;5;?\u{07}"])
    #expect(replies == "\u{1B}]4;5;rgb:dede/adad/bebe\u{07}", "got \(replies)")

    let probe = repliesFor(["\u{1B}]4;0;?\u{07}"])
    #expect(probe.hasPrefix("\u{1B}]4;0;rgb:"), "got \(probe)")
    #expect(probe.count == "\u{1B}]4;0;rgb:0000/0000/0000\u{07}".count, "got \(probe)")
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
