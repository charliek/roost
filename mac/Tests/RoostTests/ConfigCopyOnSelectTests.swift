// Config parser tests for the `copy-on-select` setting. Mirrors the
// Rust tests in `crates/roost-linux/src/config.rs::tests` so the two
// UIs agree on parsing semantics.

import Testing

@testable import Roost

@Suite("RoostConfig copy-on-select parsing")
struct ConfigCopyOnSelectTests {
    @Test func defaultsToTrue() {
        let cfg = parse("")
        #expect(cfg.copyOnSelect == .on)
    }

    @Test func parsesAllThreeStates() {
        #expect(parse("copy-on-select = off").copyOnSelect == .off)
        #expect(parse("copy-on-select = false").copyOnSelect == .off)
        #expect(parse("copy-on-select = true").copyOnSelect == .on)
        #expect(parse("copy-on-select = clipboard").copyOnSelect == .clipboard)
        #expect(parse("copy-on-select = both").copyOnSelect == .clipboard)
    }

    @Test func unknownValueKeepsDefault() {
        let cfg = parse("copy-on-select = pancakes")
        #expect(cfg.copyOnSelect == .on)
    }

    @Test func caseInsensitive() {
        #expect(parse("copy-on-select = OFF").copyOnSelect == .off)
        #expect(parse("copy-on-select = Clipboard").copyOnSelect == .clipboard)
    }
}
