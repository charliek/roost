// Config parser tests for the `clipboard-write` setting (OSC 52
// program-initiated clipboard writes). Mirrors
// `crates/roost-linux/src/config.rs::tests` 1:1 so the two UIs
// agree on parsing semantics.

import Testing

@testable import Roost

@Suite("RoostConfig clipboard-write parsing")
struct ConfigClipboardWriteTests {
    @Test func defaultsToAllow() {
        let cfg = parse("")
        #expect(cfg.clipboardWrite == .allow)
    }

    @Test func parsesAllowAndDeny() {
        #expect(parse("clipboard-write = allow").clipboardWrite == .allow)
        #expect(parse("clipboard-write = true").clipboardWrite == .allow)
        #expect(parse("clipboard-write = yes").clipboardWrite == .allow)
        #expect(parse("clipboard-write = deny").clipboardWrite == .deny)
        #expect(parse("clipboard-write = false").clipboardWrite == .deny)
        #expect(parse("clipboard-write = no").clipboardWrite == .deny)
    }

    @Test func unknownValueKeepsDefault() {
        // `ask` is phase-2 vocabulary; the current parser rejects it
        // so the default (allow) wins. Pins the contract.
        let cfg = parse("clipboard-write = ask")
        #expect(cfg.clipboardWrite == .allow)
    }

    @Test func caseInsensitive() {
        #expect(parse("clipboard-write = DENY").clipboardWrite == .deny)
        #expect(parse("clipboard-write = Allow").clipboardWrite == .allow)
    }
}
