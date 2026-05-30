// CursorShapeMappingTests — pure-helper tests for the OSC 22
// W3C cursor-name → NSCursor mapping.
//
// Strix sends `\x1b]22;pointer\x1b\\` while hovering its divider and
// `\x1b]22;default\x1b\\` to reset; kitty and friends use the broader
// W3C name set. The mapping owns the W3C-name → AppKit-cursor table;
// unknown names and the empty body both fall back to `.arrow`.

import AppKit
import Foundation
import Testing

@testable import Roost

@Suite("OSC 22 cursor shape mapping")
@MainActor
struct CursorShapeMappingTests {

    @Test("empty body → arrow (platform default)")
    func emptyBodyMapsToDefault() {
        #expect(nsCursorForW3CName("") === NSCursor.arrow)
    }

    @Test("'default' → arrow")
    func defaultName() {
        #expect(nsCursorForW3CName("default") === NSCursor.arrow)
    }

    @Test("'pointer' → pointingHand (the strix grab cursor)")
    func pointer() {
        #expect(nsCursorForW3CName("pointer") === NSCursor.pointingHand)
    }

    @Test("'text' → iBeam")
    func text() {
        #expect(nsCursorForW3CName("text") === NSCursor.iBeam)
    }

    @Test("'crosshair' → crosshair")
    func crosshair() {
        #expect(nsCursorForW3CName("crosshair") === NSCursor.crosshair)
    }

    @Test("'grab' → openHand")
    func grab() {
        #expect(nsCursorForW3CName("grab") === NSCursor.openHand)
    }

    @Test("'grabbing' → closedHand")
    func grabbing() {
        #expect(nsCursorForW3CName("grabbing") === NSCursor.closedHand)
    }

    @Test("'not-allowed' → operationNotAllowed")
    func notAllowed() {
        #expect(nsCursorForW3CName("not-allowed") === NSCursor.operationNotAllowed)
    }

    @Test("'col-resize' → resizeLeftRight")
    func colResize() {
        #expect(nsCursorForW3CName("col-resize") === NSCursor.resizeLeftRight)
    }

    @Test("'row-resize' → resizeUpDown")
    func rowResize() {
        #expect(nsCursorForW3CName("row-resize") === NSCursor.resizeUpDown)
    }

    @Test("'e-resize' → resizeRight")
    func eResize() {
        #expect(nsCursorForW3CName("e-resize") === NSCursor.resizeRight)
    }

    @Test("'w-resize' → resizeLeft")
    func wResize() {
        #expect(nsCursorForW3CName("w-resize") === NSCursor.resizeLeft)
    }

    @Test("'n-resize' → resizeUp")
    func nResize() {
        #expect(nsCursorForW3CName("n-resize") === NSCursor.resizeUp)
    }

    @Test("'s-resize' → resizeDown")
    func sResize() {
        #expect(nsCursorForW3CName("s-resize") === NSCursor.resizeDown)
    }

    @Test("unknown name → arrow (silent fallback, matches ghostty)")
    func unknownFallsBack() {
        #expect(nsCursorForW3CName("not_a_real_shape") === NSCursor.arrow)
        #expect(nsCursorForW3CName("zoom-in") === NSCursor.arrow)
    }

    // MARK: canonicalCursorShape (for the `app.cursor_shape` IPC op)

    @Test("canonical(\"\") → \"default\"")
    func canonicalEmpty() {
        #expect(canonicalCursorShape("") == "default")
    }

    @Test("canonical(\"pointer\") → \"pointer\" (passthrough)")
    func canonicalPointer() {
        #expect(canonicalCursorShape("pointer") == "pointer")
    }

    @Test("canonical(\"unknown\") → \"unknown\" (raw passthrough)")
    func canonicalUnknown() {
        // Canonical form pins the raw payload so tests can assert
        // "I asked for X; UI received X" without depending on the
        // cursor-name mapping.
        #expect(canonicalCursorShape("not_a_real_shape") == "not_a_real_shape")
    }
}
