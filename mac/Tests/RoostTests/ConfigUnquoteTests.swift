// Config parser tests for the shared value-normalization step: one
// matched quote pair stripped, no recursion, no re-trim, and a CRLF
// file parsing exactly like an LF one. Mirrors the `unquote_*` block in
// `crates/roost-linux/src/config.rs::tests` so both UIs read the same
// file the same way (plan 008 §3.1); `tabWidthsAcceptQuotedValues` has
// no Rust twin because `tab-*-width` is Mac-only.

import Testing

@testable import Roost

@Suite("RoostConfig unquote + CRLF parity")
struct ConfigUnquoteTests {
    @Test func themeAcceptsBothQuoteKinds() {
        #expect(parse("theme = \"Dracula\"").themeName == "Dracula")
        #expect(parse("theme = 'Dracula'").themeName == "Dracula")
    }

    @Test func stripsExactlyOnePair() {
        // No recursion: the inner pair survives.
        #expect(parse("theme = \"\"x\"\"").themeName == "\"x\"")
    }

    @Test func keepsMismatchedPairVerbatim() {
        // Narrower than the old set-trim, deliberately: a mismatched
        // pair is not a quoted value, so both platforms now fail the
        // theme lookup identically.
        #expect(parse("theme = \"dark'").themeName == "\"dark'")
        #expect(parse("theme = \"dark").themeName == "\"dark")
    }

    @Test func matchesScalarLevelQuotes() {
        // A combining mark right after the opening quote must not stop
        // the strip: `unquote` compares unicode scalars (like Rust), so
        // the quote matches even though the grapheme-cluster view fuses
        // it with the following mark.
        #expect(parse("theme = \"\u{0301}x\"").themeName == "\u{0301}x")
    }

    @Test func boolValueWithInteriorCRBeforeClosingQuoteParses() {
        // `"false\r"` unquotes to `false\r`; `parseBoolLike`'s trim
        // must strip that CR just like Rust's `str::trim`.
        #expect(parse("show-sidebar-agents = \"false\r\"").showSidebarAgents == false)
    }

    @Test func doesNotRetrimInteriorPadding() {
        #expect(parse("theme = \" Dracula \"").themeName == " Dracula ")
    }

    @Test func fontFamilyAcceptsSingleQuotes() {
        #expect(parse("font-family = 'JetBrains Mono'").fontFamily == "JetBrains Mono")
        #expect(parse("font-family = \"JetBrains Mono\"").fontFamily == "JetBrains Mono")
    }

    @Test func fontSizeAcceptsQuotedValue() {
        #expect(parse("font-size = \"14\"").fontSize == 14)
    }

    @Test func crlfFileParsesEveryKey() {
        let cfg = parse("theme = Dracula\r\nfont-size = 14\r\n")
        #expect(cfg.themeName == "Dracula")
        #expect(cfg.fontSize == 14)
    }

    @Test func unterminatedFinalCRLFLineParses() {
        // The last line has a bare `\r` and no `\n`; the line trim has
        // to take it off.
        let cfg = parse("font-size = 14\r\ntheme = Dracula\r")
        #expect(cfg.themeName == "Dracula")
    }

    @Test func keybindUsesTheRawValue() {
        // Pinned to the raw value on both platforms: a quoted trigger
        // stays verbatim rather than becoming a different binding.
        let cfg = parse("keybind = \"ctrl+t\" = new_tab")
        #expect(cfg.keybinds.count == 1)
        #expect(cfg.keybinds.first?.trigger == "\"ctrl+t\"")
        #expect(cfg.keybinds.first?.action == "new_tab")
    }

    @Test func tabWidthsAcceptQuotedValues() {
        let cfg = parse("tab-min-width = \"90\"\ntab-max-width = '200'\n")
        #expect(cfg.tabMinWidth == 90)
        #expect(cfg.tabMaxWidth == 200)
    }
}
