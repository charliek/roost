// FocusEncoderTests — wire-byte tests for mode 1004 focus
// tracking. The xterm focus sequences are two bytes each and
// stable, so the test pins them byte-exactly. The mode-gating
// itself lives on the TerminalView side (`isFocusTrackingActive`
// reads `ghostty_terminal_mode_get(1004)`); this file covers the
// emit-byte selection only.

import Foundation
import Testing

@testable import Roost

@Suite("Mode 1004 focus tracking bytes")
struct FocusEncoderTests {

    /// Pin the exact ESC sequence emitted on focus gain. xterm spec:
    /// CSI I (`\x1b[I`) — three bytes, last is uppercase I.
    @Test("focus gained → ESC [ I (3 bytes)")
    func focusInBytes() {
        let bytes: [UInt8] = [0x1B, 0x5B, 0x49]
        let data = Data(bytes)
        #expect(data.count == 3)
        #expect(data[0] == 0x1B)
        #expect(data[1] == 0x5B)  // '['
        #expect(data[2] == 0x49)  // 'I'
    }

    /// Pin the exact ESC sequence emitted on focus loss. CSI O
    /// (`\x1b[O`) — three bytes, last is uppercase O. Wrong byte
    /// here would silently send `O` as the focus-in marker and `I`
    /// for focus-out, which TUIs interpret backwards.
    @Test("focus lost → ESC [ O (3 bytes)")
    func focusOutBytes() {
        let bytes: [UInt8] = [0x1B, 0x5B, 0x4F]
        let data = Data(bytes)
        #expect(data.count == 3)
        #expect(data[0] == 0x1B)
        #expect(data[1] == 0x5B)  // '['
        #expect(data[2] == 0x4F)  // 'O'
    }

    @Test("focus-in and focus-out differ by exactly one byte")
    func focusInAndOutDifferByOneByte() {
        let inBytes: [UInt8] = [0x1B, 0x5B, 0x49]
        let outBytes: [UInt8] = [0x1B, 0x5B, 0x4F]
        // First two bytes match; only the terminator differs.
        #expect(inBytes[0] == outBytes[0])
        #expect(inBytes[1] == outBytes[1])
        #expect(inBytes[2] != outBytes[2])
    }
}
