// Shell-escape + drop-payload-resolver tests, Swift companion to the GTK suite
// in `crates/roost-linux/src/shell_escape.rs::tests`. The escape vectors are
// shared verbatim with the Rust side so the two drag-and-drop implementations
// stay byte-identical (the cross-UI parity the north star asks for).

import AppKit
import Foundation
import Testing

@testable import Roost

struct ShellEscapeTests {
    @Test func emptyStringIsUnchanged() {
        #expect(ShellEscape.escape("") == "")
    }

    @Test func plainPathIsUnchanged() {
        #expect(ShellEscape.escape("/Users/me/screenshots/img.png") == "/Users/me/screenshots/img.png")
    }

    @Test func spacesAreEscaped() {
        #expect(ShellEscape.escape("/Users/me/My File.png") == "/Users/me/My\\ File.png")
    }

    /// The headline case: a real macOS screenshot filename has regular spaces
    /// (escaped) and a U+202F narrow-no-break space before "PM" (passes through,
    /// being non-ASCII). Periods and digits are untouched.
    @Test func realScreenshotFilename() {
        let input = "/Users/me/Desktop/Screenshot 2026-06-28 at 3.45.12\u{202F}PM.png"
        let expected = "/Users/me/Desktop/Screenshot\\ 2026-06-28\\ at\\ 3.45.12\u{202F}PM.png"
        #expect(ShellEscape.escape(input) == expected)
    }

    @Test func backslashIsDoubled() {
        #expect(ShellEscape.escape("a\\b") == "a\\\\b")
    }

    @Test func tabIsEscaped() {
        #expect(ShellEscape.escape("a\tb") == "a\\\tb")
    }

    @Test func quotesAreEscaped() {
        #expect(ShellEscape.escape("a\"b'c`d") == "a\\\"b\\'c\\`d")
    }

    @Test func shellMetacharactersAreEscaped() {
        // Each of these is in the escape set; verify every one gets a backslash.
        #expect(ShellEscape.escape("$&;|*?(){}[]<>!#") == "\\$\\&\\;\\|\\*\\?\\(\\)\\{\\}\\[\\]\\<\\>\\!\\#")
    }

    @Test func alreadyBackslashedSpaceGetsBothEscaped() {
        // "\ " -> escape the backslash then the space.
        #expect(ShellEscape.escape("\\ ") == "\\\\\\ ")
    }

    @Test func nonAsciiPassesThrough() {
        #expect(ShellEscape.escape("/tmp/图 片.png") == "/tmp/图\\ 片.png")
    }
}

struct DropContentResolverTests {
    private func fileURL(_ path: String) -> URL {
        URL(fileURLWithPath: path)
    }

    @Test func fileURLsTakePriorityAndAreEscaped() {
        let content = TerminalView.dropContentString(
            fileURLs: [fileURL("/tmp/My File.png")],
            url: "https://example.com/x",
            string: "ignored"
        )
        #expect(content == "/tmp/My\\ File.png")
    }

    @Test func multipleFilesAreNewlineJoined() {
        let content = TerminalView.dropContentString(
            fileURLs: [fileURL("/tmp/a b.png"), fileURL("/tmp/c.png")],
            url: nil,
            string: nil
        )
        #expect(content == "/tmp/a\\ b.png\n/tmp/c.png")
    }

    @Test func webURLIsEscapedWhenNoFiles() {
        // `?` and `&` are in the escape set; `:` `/` `.` `=` are not.
        let content = TerminalView.dropContentString(
            fileURLs: [],
            url: "https://example.com/a?b=c&d=e",
            string: "ignored"
        )
        #expect(content == "https://example.com/a\\?b=c\\&d=e")
    }

    @Test func plainStringIsNotEscaped() {
        let content = TerminalView.dropContentString(
            fileURLs: [],
            url: nil,
            string: "git status && ls"
        )
        #expect(content == "git status && ls")
    }

    @Test func emptyPayloadResolvesToNil() {
        #expect(TerminalView.dropContentString(fileURLs: [], url: nil, string: nil) == nil)
        #expect(TerminalView.dropContentString(fileURLs: [], url: "", string: "") == nil)
    }

    @Test func duplicateFileURLsAreCollapsed() {
        let content = TerminalView.dropContentString(
            fileURLs: [fileURL("/tmp/shot.png"), fileURL("/tmp/shot.png")],
            url: nil,
            string: nil
        )
        #expect(content == "/tmp/shot.png")
    }

    @Test func newlineContainingPathIsDropped() {
        // A lone pathological path → no file content → nil (no stray brackets).
        #expect(TerminalView.dropContentString(
            fileURLs: [fileURL("/tmp/ev\nil.png")], url: nil, string: nil
        ) == nil)
        // Mixed with a good path → only the good one survives.
        let content = TerminalView.dropContentString(
            fileURLs: [fileURL("/tmp/ev\nil.png"), fileURL("/tmp/ok.png")],
            url: nil,
            string: nil
        )
        #expect(content == "/tmp/ok.png")
    }

    @Test func multilineTextDropIsPreserved() {
        // Plain text legitimately contains newlines (dragging multi-line text);
        // unlike file paths, it passes through unescaped.
        let content = TerminalView.dropContentString(
            fileURLs: [], url: nil, string: "line one\nline two"
        )
        #expect(content == "line one\nline two")
    }
}
