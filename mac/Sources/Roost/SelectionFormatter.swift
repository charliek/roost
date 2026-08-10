// Selection text extraction via `ghostty_formatter_*`.
//
// The formatter is the only libghostty API that can read cells outside
// the viewport, so it — not the render state — is what makes a
// scrollback-spanning copy complete. Swift mirror of
// `crates/roost-vt/src/formatter.rs`; the two must stay behaviorally
// identical.
//
// # Why this exposes exactly one function
//
// `GhosttyGridRef` is an unvalidated pin into the terminal's page list,
// and libghostty resolves a selection's endpoints with an unchecked
// `pointFromPin(...).?` (`Selection.order`). The archive is built
// `-Doptimize=ReleaseFast`, where that null unwrap is undefined
// behavior rather than a panic. Any mutating terminal call —
// `vt_write`, `resize`, `reset`, an alt-screen switch — landing between
// the pin and the format is enough to trigger it.
//
// `SelectionFormatter.text` therefore pins, formats, and frees inside a
// single synchronous call. No grid ref and no formatter handle escapes
// it, which makes the hazardous interleaving unrepresentable instead of
// merely documented.

import CGhosttyVT
import Foundation

enum SelectionFormatter {
    /// Format the inclusive cell range `start...end` of the active
    /// screen as plain text.
    ///
    /// Both endpoints are inclusive — pass the raw anchor/cursor cells,
    /// not a half-open range. Drag order does not matter: libghostty
    /// normalizes reversed endpoints itself via `Selection.order`.
    ///
    /// Returns `nil` when an endpoint no longer names a cell on the
    /// active screen — an alt-screen switch, or a row evicted from
    /// scrollback. That is an empty selection, not a failure.
    @MainActor
    static func text(
        terminal: GhosttyTerminal,
        startCol: Int,
        startScreenY: UInt32,
        endCol: Int,
        endScreenY: UInt32
    ) -> String? {
        guard let startRef = gridRef(terminal, col: startCol, screenY: startScreenY),
              let endRef = gridRef(terminal, col: endCol, screenY: endScreenY)
        else { return nil }

        var selection = GhosttySelection()
        selection.size = MemoryLayout<GhosttySelection>.size
        selection.start = startRef
        selection.end = endRef
        selection.rectangle = false

        // libghostty copies the selection into the formatter, so the
        // pointer only has to outlive `ghostty_formatter_terminal_new`
        // — but it is kept valid for the whole body anyway.
        return withUnsafePointer(to: &selection) { selectionPtr -> String? in
            var options = GhosttyFormatterTerminalOptions()
            options.size = MemoryLayout<GhosttyFormatterTerminalOptions>.size
            options.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN
            options.unwrap = false
            // Roost does want trailing spaces gone, but not
            // libghostty's version of it: its trim treats any cell
            // whose base codepoint is a space as blank, so a space
            // carrying a combining mark loses the mark and comes back
            // as a bare space. Trailing spaces are removed in
            // `trimTrailingSpaces` instead, which is otherwise
            // equivalent — textless cells are dropped either way.
            options.trim = false
            // `extra` is ignored for PLAIN, but libghostty does not
            // validate a `size` field, so every one of them still has
            // to be right or a future layout drift is misread silently.
            options.extra.size = MemoryLayout<GhosttyFormatterTerminalExtra>.size
            options.extra.screen.size = MemoryLayout<GhosttyFormatterScreenExtra>.size
            options.selection = selectionPtr

            var handle: GhosttyFormatter?
            guard ghostty_formatter_terminal_new(nil, &handle, terminal, options)
                == GHOSTTY_SUCCESS, let formatter = handle
            else { return nil }
            defer { ghostty_formatter_free(formatter) }

            var outPtr: UnsafeMutablePointer<UInt8>?
            var outLen = 0
            guard ghostty_formatter_format_alloc(formatter, nil, &outPtr, &outLen)
                == GHOSTTY_SUCCESS
            else { return nil }
            guard let outPtr else { return "" }
            defer { ghostty_free(nil, outPtr, outLen) }

            let bytes = UnsafeBufferPointer(start: outPtr, count: outLen)
            guard let raw = String(bytes: bytes, encoding: .utf8) else { return nil }
            return trimTrailingSpaces(raw)
        }
    }

    /// Drop trailing `U+0020` from every line. Only spaces — every
    /// other whitespace codepoint is content a terminal cell holds
    /// deliberately. Operates on Unicode scalars, not `Character`s, so
    /// a `\r\n` pair (one Swift `Character`) still splits on its `\n`
    /// and a space carrying a combining mark is not mistaken for a
    /// bare space.
    static func trimTrailingSpaces(_ text: String) -> String {
        var out = String.UnicodeScalarView()
        var pendingSpaces = 0
        for scalar in text.unicodeScalars {
            if scalar == " " {
                pendingSpaces += 1
                continue
            }
            if scalar != "\n" {
                for _ in 0..<pendingSpaces { out.append(" ") }
            }
            pendingSpaces = 0
            out.append(scalar)
        }
        return String(out)
    }

    /// Pin a `PointTag::Screen` cell. `nil` when the coordinate no
    /// longer names a live cell.
    @MainActor
    private static func gridRef(
        _ terminal: GhosttyTerminal,
        col: Int,
        screenY: UInt32
    ) -> GhosttyGridRef? {
        var point = GhosttyPoint()
        point.tag = GHOSTTY_POINT_TAG_SCREEN
        point.value.coordinate.x = UInt16(clamping: max(col, 0))
        point.value.coordinate.y = screenY
        var ref = GhosttyGridRef()
        ref.size = MemoryLayout<GhosttyGridRef>.size
        guard ghostty_terminal_grid_ref(terminal, point, &ref) == GHOSTTY_SUCCESS,
              ref.node != nil
        else { return nil }
        return ref
    }
}
