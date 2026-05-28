// OSC 10/11/12 query-reply synthesis tests, Swift companion to the
// Rust suite in `crates/roost-osc/src/lib.rs::tests::format_color_query_*`.
// Both ports MUST produce byte-identical replies — codex/claude-code
// see one terminal answer regardless of which UI hosts the tab. When
// the Rust suite grows a case, mirror it here.

import AppKit
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
