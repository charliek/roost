// Bracketed-paste framing tests, Swift companion to the shared suite in
// `crates/roost-ui-model/src/bracketed_paste.rs::tests`. The vectors are shared
// verbatim with the Rust side so the three UIs stay byte-identical on the
// sanitize + wrap boundary (the cross-UI parity the north star asks for).
//
// XCTest, not swift-testing, for the same reason `ShellEscapeTests` is: a swarm
// of trivially fast value-checks in the swift-testing run aborts
// `swiftpm-testing-helper` mid-run under Xcode 26.x (SIGABRT with no failing
// test — observed on CI run 30796756743 for this suite). XCTest runs in a
// separate harness and stays green.

import Foundation
import XCTest

@testable import Roost

private func wrapped(_ text: String, _ bracketed: Bool) -> [UInt8] {
    Array(wrapBracketedPaste(Data(text.utf8), bracketed: bracketed))
}

private func bytes(_ text: String) -> [UInt8] { Array(text.utf8) }

final class BracketedPasteTests: XCTestCase {
    func testEmptyTextYieldsNoBytes() {
        XCTAssertTrue(wrapped("", false).isEmpty)
        XCTAssertTrue(wrapped("", true).isEmpty)
    }

    func testPassthroughWithoutBracketedMode() {
        XCTAssertEqual(wrapped("hello\n", false), bytes("hello\n"))
        // No 2004, no region to break: the bytes are delivered verbatim.
        XCTAssertEqual(wrapped("a\u{1b}[201~b", false), bytes("a\u{1b}[201~b"))
    }

    func testPlainPayloadIsWrappedOnce() {
        XCTAssertEqual(wrapped("hello\n", true), bytes("\u{1b}[200~hello\n\u{1b}[201~"))
    }

    func testEmbeddedEndMarkerIsRemoved() {
        XCTAssertEqual(
            wrapped("\u{1b}[201~rm -rf /\n", true),
            bytes("\u{1b}[200~rm -rf /\n\u{1b}[201~")
        )
    }

    func testEmbeddedStartMarkerIsRemoved() {
        XCTAssertEqual(wrapped("a\u{1b}[200~b", true), bytes("\u{1b}[200~ab\u{1b}[201~"))
    }

    /// Removal is re-checked against the output, so halves left adjacent by an
    /// earlier removal cannot re-form a marker.
    func testRemovalCannotSpliceANewMarker() {
        XCTAssertEqual(wrapped("x\u{1b}[20\u{1b}[200~0~y", true), bytes("\u{1b}[200~xy\u{1b}[201~"))
    }

    /// Only the contiguous six-byte sequence is a marker; a truncated prefix is
    /// ordinary payload.
    func testPartialMarkerIsPreserved() {
        XCTAssertEqual(wrapped("\u{1b}[201", true), bytes("\u{1b}[200~\u{1b}[201\u{1b}[201~"))
    }

    func testUtf8AroundRemovalsIsPreserved() {
        XCTAssertEqual(wrapped("图\u{1b}[201~片", true), bytes("\u{1b}[200~图片\u{1b}[201~"))
    }
}
