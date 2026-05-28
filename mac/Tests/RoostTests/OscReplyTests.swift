// OSC 10/11/12 query-reply synthesis tests, Swift companion to the
// Rust suite in `crates/roost-osc/src/lib.rs::tests::format_color_query_*`.
// Both ports MUST produce byte-identical replies — codex/claude-code
// see one terminal answer regardless of which UI hosts the tab. When
// the Rust suite grows a case, mirror it here.

import AppKit
import CGhosttyVT
import Testing

@testable import Roost

private func asciiBytes(_ s: String) -> [UInt8] {
    Array(s.utf8)
}

@Test
func osc11_replyBgIsByteExactWithLegacy() {
    // theme bg = #1e1e1e — same value the legacy Go suite pins in
    // `internal/osc/scanner_test.go:279`.
    let bg = NSColor(srgbRed: 0x1e / 255.0, green: 0x1e / 255.0, blue: 0x1e / 255.0, alpha: 1)
    let reply = TerminalView.formatColorQueryResponse(n: 11, color: bg)
    #expect(reply.map(Array.init) == asciiBytes("\u{1B}]11;rgb:1e1e/1e1e/1e1e\u{07}"))
}

@Test
func osc10_replyFgIsByteExactWithLegacy() {
    // theme fg = #ffffff.
    let fg = NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1)
    let reply = TerminalView.formatColorQueryResponse(n: 10, color: fg)
    #expect(reply.map(Array.init) == asciiBytes("\u{1B}]10;rgb:ffff/ffff/ffff\u{07}"))
}

@Test
func osc12_replyCursorIsByteExactWithLegacy() {
    // Legacy reference: rgb:9898/9898/9d9d (the cmux/default cursor).
    let cursor = NSColor(srgbRed: 0x98 / 255.0, green: 0x98 / 255.0, blue: 0x9d / 255.0, alpha: 1)
    let reply = TerminalView.formatColorQueryResponse(n: 12, color: cursor)
    #expect(reply.map(Array.init) == asciiBytes("\u{1B}]12;rgb:9898/9898/9d9d\u{07}"))
}

@Test
func reply_rejectsUnknownQueryNumber() {
    // 13 isn't a recognised XTerm color-query code — caller treats
    // nil as "skip" rather than fall through. Mirrors the Rust
    // `format_color_query_response_rejects_unknown_n` test.
    let c = NSColor.black
    #expect(TerminalView.formatColorQueryResponse(n: 13, color: c) == nil)
    #expect(TerminalView.formatColorQueryResponse(n: 0, color: c) == nil)
}

@Test
func reply_channelOrderIsRedGreenBlue() {
    // Pin the channel order so a future format-string refactor
    // can't silently swap them. Picks distinct values per channel so
    // any swap is loud. Mirrors the Rust
    // `format_color_query_response_mixed_channels` test.
    let mixed = NSColor(srgbRed: 0x12 / 255.0, green: 0x34 / 255.0, blue: 0x56 / 255.0, alpha: 1)
    let reply = TerminalView.formatColorQueryResponse(n: 11, color: mixed)
    #expect(reply.map(Array.init) == asciiBytes("\u{1B}]11;rgb:1212/3434/5656\u{07}"))
}

/// Issue #145 regression: a mid-session `OSC 11;rgb:…` set must be
/// reflected in the next OSC 11 query reply — pre-fix the reply read
/// the static theme bg and would have returned `1c1c/1c1c/1c1c`.
@Test @MainActor
func osc11_dynamicSetIsReflectedByQueryReply() throws {
    var opts = GhosttyTerminalOptions()
    opts.cols = 80
    opts.rows = 24
    opts.max_scrollback = 0

    var maybeTerm: GhosttyTerminal?
    #expect(ghostty_terminal_new(nil, &maybeTerm, opts).rawValue == 0)
    let term = try #require(maybeTerm, "ghostty_terminal_new returned success but term is nil")
    defer { ghostty_terminal_free(term) }

    // Push a starting theme so the effective-color getters return a
    // value (libghostty's getters return GHOSTTY_NO_VALUE for any
    // color that's never been set). Mirrors the Linux test.
    let theme = Theme(
        foreground: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        background: NSColor(srgbRed: 0x1c / 255.0, green: 0x1c / 255.0, blue: 0x1c / 255.0, alpha: 1),
        cursor: NSColor(srgbRed: 0x98 / 255.0, green: 0x98 / 255.0, blue: 0x9d / 255.0, alpha: 1),
        selectionBackground: .gray,
        selectionForeground: .white,
        palette: Array(repeating: .gray, count: 256)
    )
    Theme.apply(theme, to: term)

    // Feed a mid-session `OSC 11;rgb:00/11/22` set. libghostty
    // updates its internal default-bg from this; pre-fix the reply
    // path ignored that and read the static theme bg.
    let setBytes: [UInt8] = Array("\u{1B}]11;rgb:00/11/22\u{07}".utf8)
    setBytes.withUnsafeBufferPointer {
        ghostty_terminal_vt_write(term, $0.baseAddress, setBytes.count)
    }

    let live = try #require(
        TerminalView.liveColor(forQuery: 11, terminal: term, theme: theme),
        "liveColor(forQuery: 11) returned nil"
    )
    let reply = try #require(
        TerminalView.formatColorQueryResponse(n: 11, color: live),
        "formatColorQueryResponse returned nil"
    )
    let text = String(decoding: reply, as: UTF8.self)
    #expect(
        text.contains("0000/1111/2222"),
        "reply must encode the post-set bg (got \(text))"
    )
    #expect(
        !text.contains("1c1c/1c1c/1c1c"),
        "reply must NOT encode the stale theme bg (got \(text))"
    )
}

@Test @MainActor
func osc10_dynamicSetIsReflectedByQueryReply() throws {
    var opts = GhosttyTerminalOptions()
    opts.cols = 80
    opts.rows = 24
    opts.max_scrollback = 0

    var maybeTerm: GhosttyTerminal?
    #expect(ghostty_terminal_new(nil, &maybeTerm, opts).rawValue == 0)
    let term = try #require(maybeTerm)
    defer { ghostty_terminal_free(term) }

    let theme = Theme(
        foreground: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        background: NSColor(srgbRed: 0x1c / 255.0, green: 0x1c / 255.0, blue: 0x1c / 255.0, alpha: 1),
        cursor: NSColor(srgbRed: 0x98 / 255.0, green: 0x98 / 255.0, blue: 0x9d / 255.0, alpha: 1),
        selectionBackground: .gray,
        selectionForeground: .white,
        palette: Array(repeating: .gray, count: 256)
    )
    Theme.apply(theme, to: term)

    let setBytes: [UInt8] = Array("\u{1B}]10;rgb:aa/bb/cc\u{07}".utf8)
    setBytes.withUnsafeBufferPointer {
        ghostty_terminal_vt_write(term, $0.baseAddress, setBytes.count)
    }

    let live = try #require(TerminalView.liveColor(forQuery: 10, terminal: term, theme: theme))
    let reply = try #require(TerminalView.formatColorQueryResponse(n: 10, color: live))
    let text = String(decoding: reply, as: UTF8.self)
    #expect(text.contains("aaaa/bbbb/cccc"), "got \(text)")
    #expect(!text.contains("ffff/ffff/ffff"), "stale theme fg leaked: \(text)")
}

@Test @MainActor
func osc12_dynamicSetIsReflectedByQueryReply() throws {
    var opts = GhosttyTerminalOptions()
    opts.cols = 80
    opts.rows = 24
    opts.max_scrollback = 0

    var maybeTerm: GhosttyTerminal?
    #expect(ghostty_terminal_new(nil, &maybeTerm, opts).rawValue == 0)
    let term = try #require(maybeTerm)
    defer { ghostty_terminal_free(term) }

    let theme = Theme(
        foreground: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        background: NSColor(srgbRed: 0x1c / 255.0, green: 0x1c / 255.0, blue: 0x1c / 255.0, alpha: 1),
        cursor: NSColor(srgbRed: 0x98 / 255.0, green: 0x98 / 255.0, blue: 0x9d / 255.0, alpha: 1),
        selectionBackground: .gray,
        selectionForeground: .white,
        palette: Array(repeating: .gray, count: 256)
    )
    Theme.apply(theme, to: term)

    let setBytes: [UInt8] = Array("\u{1B}]12;rgb:de/ad/be\u{07}".utf8)
    setBytes.withUnsafeBufferPointer {
        ghostty_terminal_vt_write(term, $0.baseAddress, setBytes.count)
    }

    let live = try #require(TerminalView.liveColor(forQuery: 12, terminal: term, theme: theme))
    let reply = try #require(TerminalView.formatColorQueryResponse(n: 12, color: live))
    let text = String(decoding: reply, as: UTF8.self)
    #expect(text.contains("dede/adad/bebe"), "got \(text)")
    #expect(!text.contains("9898/9898/9d9d"), "stale theme cursor leaked: \(text)")
}
