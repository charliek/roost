// Config parser tests for the `word-break-chars` setting. Mirrors
// `crates/roost-linux/src/config.rs::tests::word_break_chars_*` (PR B)
// so both UIs agree on the wire surface.
//
// The key is named `word-break-chars` for Ghostty compatibility, but
// the value is treated as the EXTRA word-char set (chars that count
// as word chars beyond Unicode letters/digits) — see
// `WordSelection.swift` for the rationale.

import Testing

@testable import Roost

@Suite("RoostConfig word-break-chars parsing")
struct ConfigWordBreakCharsTests {
    @Test func defaultsToGhosttySet() {
        let cfg = parse("")
        #expect(cfg.wordBreakChars == "_-.+~/:@%")
    }

    @Test func acceptsOverride() {
        let cfg = parse("word-break-chars = _-")
        #expect(cfg.wordBreakChars == "_-")
    }

    @Test func emptyValueDisablesExtras() {
        // Explicit empty value means "Unicode letters/digits only" —
        // a deliberate user choice distinct from "missing key".
        let cfg = parse("word-break-chars = ")
        #expect(cfg.wordBreakChars == "")
    }

    @Test func mixedWithOtherKeys() {
        // Pin parse-order independence: word-break-chars sits next to
        // copy-on-select in the file without affecting either.
        let cfg = parse(
            """
            copy-on-select = off
            word-break-chars = _-
            theme = catppuccin-mocha
            """
        )
        #expect(cfg.wordBreakChars == "_-")
        #expect(cfg.copyOnSelect == .off)
        #expect(cfg.themeName == "catppuccin-mocha")
    }
}
