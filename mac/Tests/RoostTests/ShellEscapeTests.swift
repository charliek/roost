// Shell-escape + drop-payload-resolver tests, Swift companion to the suite
// in `crates/roost-ui-model/src/shell_escape.rs::tests` (shared with iced).
// The escape vectors are shared verbatim with the Rust side so the two
// drag-and-drop implementations
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

    /// Shared verbatim with the Rust `escape_byte_passes_through` vector: the
    /// escaper never drops input (that would make the escaped string name a
    /// different file), so ESC survives and only the `[` after it is escaped.
    /// ESC-bearing paths are rejected up in `TerminalView.dropContentString`.
    func testEscapeBytePassesThrough() {
        XCTAssertEqual(
            ShellEscape.escape("/tmp/ev\u{1B}[201~il.png"),
            "/tmp/ev\u{1B}\\[201~il.png"
        )
        XCTAssertEqual(ShellEscape.escape("\u{1B}"), "\u{1B}")
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

    /// Shared with the Rust `url_is_escaped_when_no_safe_path_remains` vector.
    func testWebURLIsEscapedWhenNoFiles() {
        // `?` and `&` are in the escape set; `:` `/` `.` `=` are not.
        XCTAssertEqual(
            TerminalView.dropContentString(
                fileURLs: [], url: "https://example.com/a?b=c&d=e", string: "ignored"
            ),
            "https://example.com/a\\?b=c\\&d=e"
        )
    }

    /// Shared with the Rust `control_bearing_url_falls_through_to_text` vector:
    /// a rejected URL is absent, not stripped, so the deliberately unfiltered
    /// string fallback answers instead.
    func testControlBearingURLFallsThroughToString() {
        for control in ["\n", "\u{0B}", "\u{0C}", "\r", "\u{85}", "\u{2028}", "\u{2029}", "\u{1B}"] {
            XCTAssertEqual(
                TerminalView.dropContentString(
                    fileURLs: [], url: "https://example.com/\(control)evil", string: "fallback"
                ),
                "fallback",
                "url bearing \(control.unicodeScalars.map(\.value)) should fall through"
            )
        }
    }

    /// Shared with the Rust `control_bearing_url_and_text_yields_raw_text`
    /// vector. Documents the accepted #282 baseline: a drag can populate both
    /// `.URL` and `.string` with the same control-bearing text, and the
    /// rejected URL then falls through to the deliberately unfiltered string
    /// arm, so the raw text reaches the PTY. That plain-text boundary is owned
    /// by #280's bracketed-paste mitigations — and because we reject rather
    /// than strip, the URL arm must not launder the payload into an escaped
    /// form here either.
    func testControlBearingURLAndStringYieldsRawString() {
        let payload = "https://example.com/\u{1B}[201~evil"
        XCTAssertEqual(
            TerminalView.dropContentString(fileURLs: [], url: payload, string: payload),
            payload
        )
    }

    /// Shared with the Rust `control_bearing_url_without_text_is_none` vector.
    func testControlBearingURLWithoutStringIsNil() {
        XCTAssertNil(
            TerminalView.dropContentString(
                fileURLs: [], url: "https://example.com/\u{1B}[201~evil", string: nil
            )
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

    /// Shared with the Rust `newline_and_carriage_return_paths_are_rejected`
    /// vector.
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
        XCTAssertNil(
            TerminalView.dropContentString(fileURLs: [fileURL("/tmp/ev\ril.png")], url: nil, string: nil)
        )
    }

    /// Shared with the Rust `vertical_tab_bearing_paths_are_rejected` vector.
    func testVerticalTabBearingPathIsDropped() {
        XCTAssertNil(
            TerminalView.dropContentString(
                fileURLs: [fileURL("/tmp/ev\u{0B}il.png")], url: nil, string: nil
            )
        )
        XCTAssertEqual(
            TerminalView.dropContentString(
                fileURLs: [fileURL("/tmp/ev\u{0B}il.png"), fileURL("/tmp/ok.png")],
                url: nil, string: nil
            ),
            "/tmp/ok.png"
        )
    }

    /// Shared with the Rust `form_feed_bearing_paths_are_rejected` vector.
    func testFormFeedBearingPathIsDropped() {
        XCTAssertNil(
            TerminalView.dropContentString(
                fileURLs: [fileURL("/tmp/ev\u{0C}il.png")], url: nil, string: nil
            )
        )
        XCTAssertEqual(
            TerminalView.dropContentString(
                fileURLs: [fileURL("/tmp/ev\u{0C}il.png"), fileURL("/tmp/ok.png")],
                url: nil, string: nil
            ),
            "/tmp/ok.png"
        )
    }

    /// Shared with the Rust `unicode_newline_bearing_paths_are_rejected`
    /// vector: NEL, LS and PS are `isNewline` scalars too.
    func testUnicodeNewlineBearingPathIsDropped() {
        for control in ["\u{85}", "\u{2028}", "\u{2029}"] {
            XCTAssertNil(
                TerminalView.dropContentString(
                    fileURLs: [fileURL("/tmp/ev\(control)il.png")], url: nil, string: nil
                ),
                "path bearing \(control.unicodeScalars.map(\.value)) should be dropped"
            )
        }
        XCTAssertEqual(
            TerminalView.dropContentString(
                fileURLs: [
                    fileURL("/tmp/ev\u{85}il.png"),
                    fileURL("/tmp/ev\u{2028}il.png"),
                    fileURL("/tmp/ev\u{2029}il.png"),
                    fileURL("/tmp/ok.png"),
                ],
                url: nil, string: nil
            ),
            "/tmp/ok.png"
        )
    }

    /// Shared with the Rust `escape_bearing_paths_are_rejected` vector: an ESC
    /// in a filename is rejected at the drop boundary rather than stripped by
    /// the escaper.
    func testControlBearingPathIsDropped() {
        XCTAssertNil(
            TerminalView.dropContentString(
                fileURLs: [fileURL("/tmp/ev\u{1B}[201~il.png")], url: nil, string: nil
            )
        )
        XCTAssertEqual(
            TerminalView.dropContentString(
                fileURLs: [fileURL("/tmp/ev\u{1B}[201~il.png"), fileURL("/tmp/ok.png")],
                url: nil, string: nil
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
