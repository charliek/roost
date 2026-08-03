// Bracketed-paste framing tests, Swift companion to the shared suite in
// `crates/roost-ui-model/src/bracketed_paste.rs::tests`. The vectors are shared
// verbatim with the Rust side so the three UIs stay byte-identical on the
// sanitize + wrap boundary (the cross-UI parity the north star asks for).

import Foundation
import Testing

@testable import Roost

private func wrapped(_ text: String, _ bracketed: Bool) -> [UInt8] {
    Array(wrapBracketedPaste(Data(text.utf8), bracketed: bracketed))
}

private func bytes(_ text: String) -> [UInt8] { Array(text.utf8) }

@Suite struct BracketedPasteTests {
    @Test func emptyTextYieldsNoBytes() {
        #expect(wrapped("", false).isEmpty)
        #expect(wrapped("", true).isEmpty)
    }

    @Test func passthroughWithoutBracketedMode() {
        #expect(wrapped("hello\n", false) == bytes("hello\n"))
        // No 2004, no region to break: the bytes are delivered verbatim.
        #expect(wrapped("a\u{1b}[201~b", false) == bytes("a\u{1b}[201~b"))
    }

    @Test func plainPayloadIsWrappedOnce() {
        #expect(wrapped("hello\n", true) == bytes("\u{1b}[200~hello\n\u{1b}[201~"))
    }

    @Test func embeddedEndMarkerIsRemoved() {
        #expect(
            wrapped("\u{1b}[201~rm -rf /\n", true) == bytes("\u{1b}[200~rm -rf /\n\u{1b}[201~")
        )
    }

    @Test func embeddedStartMarkerIsRemoved() {
        #expect(wrapped("a\u{1b}[200~b", true) == bytes("\u{1b}[200~ab\u{1b}[201~"))
    }

    /// Removal is re-checked against the output, so halves left adjacent by an
    /// earlier removal cannot re-form a marker.
    @Test func removalCannotSpliceANewMarker() {
        #expect(wrapped("x\u{1b}[20\u{1b}[200~0~y", true) == bytes("\u{1b}[200~xy\u{1b}[201~"))
    }

    /// Only the contiguous six-byte sequence is a marker; a truncated prefix is
    /// ordinary payload.
    @Test func partialMarkerIsPreserved() {
        #expect(wrapped("\u{1b}[201", true) == bytes("\u{1b}[200~\u{1b}[201\u{1b}[201~"))
    }

    @Test func utf8AroundRemovalsIsPreserved() {
        #expect(wrapped("图\u{1b}[201~片", true) == bytes("\u{1b}[200~图片\u{1b}[201~"))
    }
}
