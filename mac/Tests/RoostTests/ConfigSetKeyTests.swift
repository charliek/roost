// Round-trip tests for `RoostConfig.setKey` / `renderSetKey`.
// Mirrors `crates/roost-linux/src/config.rs::tests` (the
// `set_key_*` cases) so the two UIs agree on write-back semantics.

import Foundation
import Testing

@testable import Roost

@Suite("RoostConfig.setKey round-trip")
struct ConfigSetKeyTests {
    @Test func replacesExistingValueInPlace() {
        let before = "theme = catppuccin-mocha\nfont-size = 14\n"
        let after = RoostConfig.renderSetKey(existing: before, key: "theme", value: "roost-dark")
        // `theme` updated; `font-size` untouched; trailing newline kept.
        #expect(after == "theme = roost-dark\nfont-size = 14\n")
    }

    @Test func appendsWhenMissing() {
        let before = "theme = roost-dark\n"
        let after = RoostConfig.renderSetKey(
            existing: before,
            key: "font-family",
            value: "\"JetBrains Mono\""
        )
        #expect(after == "theme = roost-dark\nfont-family = \"JetBrains Mono\"\n")
    }

    @Test func appendsToEmptyFile() {
        let after = RoostConfig.renderSetKey(existing: "", key: "theme", value: "roost-dark")
        // No leading blank line — empty file is treated as zero rows.
        #expect(after == "theme = roost-dark\n")
    }

    @Test func appendsWhenNoTrailingNewline() {
        // The file ended without a newline (hand-edited). The new line
        // still lands on its own row.
        let before = "theme = roost-dark"
        let after = RoostConfig.renderSetKey(existing: before, key: "font-size", value: "14")
        #expect(after == "theme = roost-dark\nfont-size = 14\n")
    }

    @Test func replacesAllDuplicates() {
        // The parser is "last-wins" on duplicates, so replacing only
        // the first occurrence would let a stale later line clobber
        // the new value. Every occurrence must be rewritten.
        let before = "theme = a\ntheme = b\nfont-size = 14\ntheme = c\n"
        let after = RoostConfig.renderSetKey(existing: before, key: "theme", value: "roost-dark")
        #expect(
            after
                == "theme = roost-dark\ntheme = roost-dark\nfont-size = 14\ntheme = roost-dark\n"
        )
    }

    @Test func preservesCommentsAndOtherKeys() {
        let before = "# my roost config\n\ntheme = old\n# inline note\nfont-size = 14\n"
        let after = RoostConfig.renderSetKey(existing: before, key: "theme", value: "new")
        #expect(after == "# my roost config\n\ntheme = new\n# inline note\nfont-size = 14\n")
    }

    @Test func ignoresCommentedLines() {
        // A `# theme = …` line shouldn't be treated as the canonical
        // setting; we append rather than uncomment the user's disabled
        // entry.
        let before = "# theme = disabled\nfont-size = 14\n"
        let after = RoostConfig.renderSetKey(existing: before, key: "theme", value: "roost-dark")
        #expect(after == "# theme = disabled\nfont-size = 14\ntheme = roost-dark\n")
    }

    @Test func valueWithSpacesRoundTripsViaCallerQuoting() {
        // `setKey` writes `value` verbatim; quoting (when the value
        // contains spaces) is the caller's responsibility. The parser
        // strips matching surrounding quotes on read, so a round-trip
        // re-parses cleanly.
        let after = RoostConfig.renderSetKey(
            existing: "",
            key: "font-family",
            value: "\"JetBrains Mono\""
        )
        let cfg = parse(after)
        #expect(cfg.fontFamily == "JetBrains Mono")
    }

    @Test func preservesLeadingWhitespaceInUnrelatedLines() {
        // Indented unrelated lines should round-trip exactly (we only
        // rewrite the matched key's line).
        let before = "    # indented note\n  font-size = 14\ntheme = old\n"
        let after = RoostConfig.renderSetKey(existing: before, key: "theme", value: "new")
        #expect(after == "    # indented note\n  font-size = 14\ntheme = new\n")
    }

    @Test func preservesIndentOnMatchedLine() {
        let before = "  theme = old\nfont-size = 14\n"
        let after = RoostConfig.renderSetKey(existing: before, key: "theme", value: "new")
        #expect(after == "  theme = new\nfont-size = 14\n")
    }

    @Test func diskRoundTripCreatesParentDir() throws {
        // Disk-level smoke: writes through a missing intermediate
        // directory and round-trips through the parser.
        let tmp = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: tmp) }
        let path = tmp.appendingPathComponent("nested/dir/config.conf")
        #expect(RoostConfig.setKey("theme", value: "roost-dark", at: path) == nil)
        #expect(
            RoostConfig.setKey("font-family", value: "\"JetBrains Mono\"", at: path) == nil
        )
        #expect(RoostConfig.setKey("font-size", value: "15", at: path) == nil)
        let cfg = RoostConfig.load(from: path)
        #expect(cfg.themeName == "roost-dark")
        #expect(cfg.fontFamily == "JetBrains Mono")
        #expect(cfg.fontSize == 15)
    }

    private func makeTempDir() throws -> URL {
        let dir = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("RoostConfigSetKeyTests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }
}
