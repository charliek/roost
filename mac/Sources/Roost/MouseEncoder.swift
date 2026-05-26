// libghostty-vt mouse encoder bridge for the Mac UI.
//
// Sibling of KeyEncoder.swift. Translates a scroll-wheel notch into a
// mouse button-4 (up) / button-5 (down) report when the focused app has
// enabled mouse tracking (DECSET 1000/1002/1003 + SGR 1006). Before this
// existed, TerminalView.scrollWheel dropped the wheel under tracking, so
// opencode / htop / etc. couldn't be scrolled with the wheel.
//
// One MouseEncoder per TerminalView, allocated alongside the KeyEncoder
// and freed in lockstep. `setopt_from_terminal` runs before every encode
// so tracking mode + output format follow the live terminal; the size
// context (cell geometry) is pushed per encode too since it can change
// on font-size / window resize.

import AppKit
import CGhosttyVT
import Foundation

@MainActor
final class MouseEncoder {
    /// `nonisolated(unsafe)` for the same reason as KeyEncoder's handles:
    /// opaque libghostty-vt pointers aren't Sendable, but they're only
    /// ever touched on the main thread (allocated in init, read in
    /// encode, freed in deinit alongside the owning TerminalView).
    nonisolated(unsafe) private let encoder: GhosttyMouseEncoder
    nonisolated(unsafe) private let event: GhosttyMouseEvent
    nonisolated(unsafe) private let terminal: GhosttyTerminal

    init?(terminal: GhosttyTerminal) {
        var enc: GhosttyMouseEncoder?
        let rcEncoder = ghostty_mouse_encoder_new(nil, &enc)
        guard rcEncoder.rawValue == GHOSTTY_SUCCESS.rawValue, let enc else {
            return nil
        }
        var ev: GhosttyMouseEvent?
        let rcEvent = ghostty_mouse_event_new(nil, &ev)
        guard rcEvent.rawValue == GHOSTTY_SUCCESS.rawValue, let ev else {
            ghostty_mouse_encoder_free(enc)
            return nil
        }
        self.encoder = enc
        self.event = ev
        self.terminal = terminal
    }

    deinit {
        ghostty_mouse_event_free(event)
        ghostty_mouse_encoder_free(encoder)
    }

    /// Encode a single wheel notch as a button press at the pointer's
    /// cell. `x`/`y` are in surface-space pixels (points are fine as long
    /// as `cellWidth`/`cellHeight` use the same unit — the encoder only
    /// divides position by cell size to find the cell). Returns empty
    /// Data when the encoder declines to report (e.g. tracking off).
    func encodeWheel(
        button: GhosttyMouseButton,
        mods: GhosttyMods,
        x: Float,
        y: Float,
        screenWidth: UInt32,
        screenHeight: UInt32,
        cellWidth: UInt32,
        cellHeight: UInt32
    ) -> Data {
        ghostty_mouse_encoder_setopt_from_terminal(encoder, terminal)

        var size = GhosttyMouseEncoderSize()
        size.size = MemoryLayout<GhosttyMouseEncoderSize>.size
        size.screen_width = screenWidth
        size.screen_height = screenHeight
        size.cell_width = max(cellWidth, 1)   // C API rejects a zero cell size
        size.cell_height = max(cellHeight, 1)
        withUnsafePointer(to: &size) {
            ghostty_mouse_encoder_setopt(encoder, GHOSTTY_MOUSE_ENCODER_OPT_SIZE, $0)
        }

        ghostty_mouse_event_set_action(event, GHOSTTY_MOUSE_ACTION_PRESS)
        ghostty_mouse_event_set_button(event, button)
        ghostty_mouse_event_set_mods(event, mods)
        var pos = GhosttyMousePosition()
        pos.x = x
        pos.y = y
        ghostty_mouse_event_set_position(event, pos)

        // Stack buffer first; grow on OUT_OF_SPACE (mirrors KeyEncoder).
        var stackBuf = [CChar](repeating: 0, count: 64)
        var written: size_t = 0
        let rc = stackBuf.withUnsafeMutableBufferPointer { buf in
            ghostty_mouse_encoder_encode(encoder, event, buf.baseAddress, buf.count, &written)
        }
        if rc.rawValue == GHOSTTY_SUCCESS.rawValue {
            return Self.dataFromCChars(stackBuf, count: written)
        }
        guard written > 0 else { return Data() }
        var dynBuf = [CChar](repeating: 0, count: written)
        let rc2 = dynBuf.withUnsafeMutableBufferPointer { buf in
            ghostty_mouse_encoder_encode(encoder, event, buf.baseAddress, buf.count, &written)
        }
        if rc2.rawValue == GHOSTTY_SUCCESS.rawValue {
            return Self.dataFromCChars(dynBuf, count: written)
        }
        return Data()
    }

    private static func dataFromCChars(_ buf: [CChar], count: size_t) -> Data {
        guard count > 0 else { return Data() }
        return buf.prefix(count).withUnsafeBufferPointer { ptr in
            Data(bytes: ptr.baseAddress!, count: count)
        }
    }
}
