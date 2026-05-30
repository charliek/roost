// FocusEncoderTests — wire-byte tests for mode 1004 focus
// tracking. Drives the production `TerminalView.encodeFocusBytes`
// helper (which routes through libghostty-vt's
// `ghostty_focus_encode`) and asserts on its output. Test-local
// literals are only used to PIN the canonical xterm sequences
// against the encoder's actual output — a regression that swapped
// CSI I / CSI O would fail here.

import Foundation
import Testing

@testable import Roost

@Suite("Mode 1004 focus tracking bytes")
struct FocusEncoderTests {

    /// Focus gained → CSI I (`\x1b[I`) — three bytes. Asserts the
    /// canonical xterm sequence against the production encoder
    /// output, so a wrong byte in `ghostty_focus_encode` (or a
    /// swapped event mapping in `encodeFocusBytes`) is caught here.
    @Test("focus gained → ESC [ I (3 bytes from encoder)")
    func focusInBytes() {
        let produced = TerminalView.encodeFocusBytes(focused: true)
        #expect(produced == Data([0x1B, 0x5B, 0x49]))
    }

    /// Focus lost → CSI O (`\x1b[O`) — three bytes. A regression
    /// that emitted `O` for gained and `I` for lost would silently
    /// invert focus-state on every TUI that uses mode 1004.
    @Test("focus lost → ESC [ O (3 bytes from encoder)")
    func focusOutBytes() {
        let produced = TerminalView.encodeFocusBytes(focused: false)
        #expect(produced == Data([0x1B, 0x5B, 0x4F]))
    }

    /// The two encoder outputs differ by exactly one byte (the
    /// terminator: I vs O). Asserts on the encoder-produced byte
    /// arrays, so a future change that grew the sequence (e.g.
    /// added a parameter) would fail loudly.
    @Test("focus-in and focus-out encoder output differ by exactly one byte")
    func focusInAndOutDifferByOneByte() {
        let gained = Array(TerminalView.encodeFocusBytes(focused: true))
        let lost = Array(TerminalView.encodeFocusBytes(focused: false))
        #expect(gained.count == lost.count)
        #expect(gained.count == 3)
        // First two bytes (ESC + `[`) match; only the terminator
        // differs.
        #expect(gained[0] == lost[0])
        #expect(gained[1] == lost[1])
        #expect(gained[2] != lost[2])
    }
}
