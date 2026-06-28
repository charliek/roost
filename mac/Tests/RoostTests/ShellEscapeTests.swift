// Shell-escape + drop-payload-resolver tests, Swift companion to the GTK suite
// in `crates/roost-linux/src/shell_escape.rs::tests`. The escape vectors are
// shared verbatim with the Rust side so the two drag-and-drop implementations
// stay byte-identical (the cross-UI parity the north star asks for).
//
// XCTest, not swift-testing (the repo's usual choice): a swarm of trivially
// fast value-checks added to the swift-testing run reliably SIGTRAPs
// `swiftpm-testing-helper` mid-run under Xcode 26.x (a known runner bug — the
// same class of failure that forces the `.disabled("...crashes the
// swift-testing runner...")` tests elsewhere in this suite). XCTest runs in a
// separate harness, so these stay green without destabilizing the rest.

import AppKit
import Foundation
import XCTest

@testable import Roost

final class ShellEscapeTests: XCTestCase {
    func testLeavesSafeTextUnchanged() {
        XCTAssertEqual(ShellEscape.escape(""), "")
        XCTAssertEqual(
            ShellEscape.escape("/Users/me/screenshots/img.png"),
            "/Users/me/screenshots/img.png"
        )
        // Non-ASCII (incl. the U+202F narrow-no-break space) passes through.
        XCTAssertEqual(ShellEscape.escape("/tmp/图 片.png"), "/tmp/图\\ 片.png")
    }

    func testEscapesSpaces() {
        XCTAssertEqual(ShellEscape.escape("/Users/me/My File.png"), "/Users/me/My\\ File.png")
        // Real macOS screenshot name: regular spaces escaped, the U+202F before
        // "PM" and the periods/digits untouched.
        XCTAssertEqual(
            ShellEscape.escape("/Users/me/Desktop/Screenshot 2026-06-28 at 3.45.12\u{202F}PM.png"),
            "/Users/me/Desktop/Screenshot\\ 2026-06-28\\ at\\ 3.45.12\u{202F}PM.png"
        )
    }

    func testEscapesBackslashTabQuotesAndMetacharacters() {
        XCTAssertEqual(ShellEscape.escape("a\\b"), "a\\\\b") // backslash doubled
        XCTAssertEqual(ShellEscape.escape("a\tb"), "a\\\tb") // tab
        XCTAssertEqual(ShellEscape.escape("a\"b'c`d"), "a\\\"b\\'c\\`d") // quotes
        XCTAssertEqual(
            ShellEscape.escape("$&;|*?(){}[]<>!#"),
            "\\$\\&\\;\\|\\*\\?\\(\\)\\{\\}\\[\\]\\<\\>\\!\\#"
        )
        // "\ " -> escape the backslash, then the space.
        XCTAssertEqual(ShellEscape.escape("\\ "), "\\\\\\ ")
    }
}

@MainActor
final class DropContentResolverTests: XCTestCase {
    private func fileURL(_ path: String) -> URL { URL(fileURLWithPath: path) }

    func testFileURLsTakePriorityAndAreEscaped() {
        XCTAssertEqual(
            TerminalView.dropContentString(
                fileURLs: [fileURL("/tmp/My File.png")], url: "https://example.com/x", string: "ignored"
            ),
            "/tmp/My\\ File.png"
        )
    }

    func testMultipleFilesAreNewlineJoined() {
        XCTAssertEqual(
            TerminalView.dropContentString(
                fileURLs: [fileURL("/tmp/a b.png"), fileURL("/tmp/c.png")], url: nil, string: nil
            ),
            "/tmp/a\\ b.png\n/tmp/c.png"
        )
    }

    func testWebURLIsEscapedWhenNoFiles() {
        // `?` and `&` are in the escape set; `:` `/` `.` `=` are not.
        XCTAssertEqual(
            TerminalView.dropContentString(
                fileURLs: [], url: "https://example.com/a?b=c&d=e", string: "ignored"
            ),
            "https://example.com/a\\?b=c\\&d=e"
        )
    }

    func testPlainStringIsNotEscaped() {
        XCTAssertEqual(
            TerminalView.dropContentString(fileURLs: [], url: nil, string: "git status && ls"),
            "git status && ls"
        )
    }

    func testDuplicateFileURLsAreCollapsed() {
        XCTAssertEqual(
            TerminalView.dropContentString(
                fileURLs: [fileURL("/tmp/shot.png"), fileURL("/tmp/shot.png")], url: nil, string: nil
            ),
            "/tmp/shot.png"
        )
    }

    func testNewlineBearingPathIsDropped() {
        // A lone pathological path → nil (no stray brackets).
        XCTAssertNil(
            TerminalView.dropContentString(fileURLs: [fileURL("/tmp/ev\nil.png")], url: nil, string: nil)
        )
        // Mixed with a good path → only the good one survives.
        XCTAssertEqual(
            TerminalView.dropContentString(
                fileURLs: [fileURL("/tmp/ev\nil.png"), fileURL("/tmp/ok.png")], url: nil, string: nil
            ),
            "/tmp/ok.png"
        )
    }

    func testMultilineTextDropIsPreserved() {
        // Plain text legitimately keeps its newlines (multi-line text drop).
        XCTAssertEqual(
            TerminalView.dropContentString(fileURLs: [], url: nil, string: "line one\nline two"),
            "line one\nline two"
        )
    }

    func testEmptyPayloadResolvesToNil() {
        XCTAssertNil(TerminalView.dropContentString(fileURLs: [], url: nil, string: nil))
        XCTAssertNil(TerminalView.dropContentString(fileURLs: [], url: "", string: ""))
    }
}
