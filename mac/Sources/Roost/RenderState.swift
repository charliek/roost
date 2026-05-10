// Swift wrapper around libghostty-vt's render-state surface.
//
// Phase 5.4b scope: lifecycle (new/update/free) + default-colors
// readback. Per-cell walking lands in 5.4c.
//
// libghostty-vt's render state is a pull model: you call
// `ghostty_render_state_update` once per frame to snapshot the
// terminal, then read whatever you need (default colors, row + cell
// iterators, etc.). Snapshots are cheap; the expensive thing is
// walking 80x24 cells, which 5.4c will do.
//
// The Go cgo binding solved the same problem in
// `internal/ghostty/render.go`; this is a Swift port of just the
// pieces we need so far. Field-name parity with the upstream C
// header (size, foreground, background) is checked at compile time
// by the Swift importer.

import AppKit
import CGhosttyVT

final class RenderState {
    /// Opaque libghostty-vt render-state handle. Allocated on
    /// `init`, freed in `deinit`. Same `nonisolated(unsafe)`
    /// concurrency story as TerminalView.terminal — see comment there.
    nonisolated(unsafe) private var rs: GhosttyRenderState?

    /// Row iterator allocated once and reused per frame. Reset by
    /// `ghostty_render_state_get(rs, ROW_ITERATOR, &rowIter)` at the
    /// start of each `walk()`.
    nonisolated(unsafe) private var rowIter: GhosttyRenderStateRowIterator?

    /// Cell iterator allocated once and reused per row.
    nonisolated(unsafe) private var cells: GhosttyRenderStateRowCells?

    init() {
        var rsHandle: GhosttyRenderState?
        if ghostty_render_state_new(nil, &rsHandle).rawValue != 0 || rsHandle == nil {
            fatalError("ghostty_render_state_new failed")
        }

        var rowIterHandle: GhosttyRenderStateRowIterator?
        if ghostty_render_state_row_iterator_new(nil, &rowIterHandle).rawValue != 0
            || rowIterHandle == nil
        {
            ghostty_render_state_free(rsHandle!)
            fatalError("ghostty_render_state_row_iterator_new failed")
        }

        var cellsHandle: GhosttyRenderStateRowCells?
        if ghostty_render_state_row_cells_new(nil, &cellsHandle).rawValue != 0
            || cellsHandle == nil
        {
            ghostty_render_state_row_iterator_free(rowIterHandle!)
            ghostty_render_state_free(rsHandle!)
            fatalError("ghostty_render_state_row_cells_new failed")
        }

        self.rs = rsHandle
        self.rowIter = rowIterHandle
        self.cells = cellsHandle
    }

    deinit {
        if let cells {
            ghostty_render_state_row_cells_free(cells)
        }
        if let rowIter {
            ghostty_render_state_row_iterator_free(rowIter)
        }
        if let rs {
            ghostty_render_state_free(rs)
        }
    }

    /// Snapshot the terminal's current state into this render state.
    /// Call once per frame before reading colors / walking cells.
    /// Must be called on the same thread as the terminal (main).
    func update(terminal: GhosttyTerminal) {
        guard let rs else { return }
        let rc = ghostty_render_state_update(rs, terminal)
        if rc.rawValue != 0 {
            // Update failures are unexpected at this stage of the
            // refactor — bail loudly so we notice in dev. Promotes to
            // a real error path once the renderer has a recovery
            // story (probably "skip this frame").
            fatalError("ghostty_render_state_update failed (rc.rawValue=\(rc.rawValue))")
        }
    }

    /// Walk every cell in the latest snapshot. The callback receives
    /// the row + column index plus an optional explicit background
    /// color — `nil` means "use the default bg" (the renderer should
    /// not redraw those cells against the default canvas).
    ///
    /// The callback runs once per cell that the row iterator emits.
    /// It must NOT call any other RenderState method — the iterators
    /// are reused per frame and re-entrancy would corrupt state.
    ///
    /// 5.4d will extend the callback to also receive a glyph
    /// codepoint (read via the GRAPHEMES_BUF data tag).
    func walk(_ fn: (_ row: Int, _ col: Int, _ background: NSColor?) -> Void) {
        guard let rs, let rowIter, let cells else { return }

        // Reset the row iterator from the latest snapshot. The C
        // function takes a `void *` that we point at our handle slot
        // so the call can re-attach it to the new frame.
        if withUnsafeMutablePointer(to: &self.rowIter, { ptr in
            ghostty_render_state_get(
                rs,
                GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                UnsafeMutableRawPointer(ptr)
            )
        }).rawValue != 0 {
            return
        }

        var row = -1
        while ghostty_render_state_row_iterator_next(rowIter) {
            row += 1

            if withUnsafeMutablePointer(to: &self.cells, { ptr in
                ghostty_render_state_row_get(
                    rowIter,
                    GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                    UnsafeMutableRawPointer(ptr)
                )
            }).rawValue != 0 {
                continue
            }

            var col = -1
            while ghostty_render_state_row_cells_next(cells) {
                col += 1

                // Background color is optional per cell: only reads
                // back GHOSTTY_SUCCESS when the cell has an explicit
                // bg (e.g. erase-with-color). Default-bg cells return
                // a non-zero rc and we treat that as "no override".
                var bg = GhosttyColorRgb()
                let rc = ghostty_render_state_row_cells_get(
                    cells,
                    GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                    &bg
                )
                let background: NSColor? =
                    rc.rawValue == 0 ? nsColor(bg) : nil
                fn(row, col, background)
            }
        }
    }

    /// The terminal's current default foreground + background colors.
    /// `GhosttyRenderStateColors` is a sized struct (the C header's
    /// versioning convention); we set `size` to `MemoryLayout.size`
    /// before each call so libghostty-vt knows how much of the
    /// struct we understand.
    func defaultColors() -> (foreground: NSColor, background: NSColor) {
        guard let rs else {
            return (.white, .black)
        }
        var colors = GhosttyRenderStateColors()
        colors.size = MemoryLayout<GhosttyRenderStateColors>.size
        let rc = ghostty_render_state_colors_get(rs, &colors)
        if rc.rawValue != 0 {
            return (.white, .black)
        }
        return (
            foreground: nsColor(colors.foreground),
            background: nsColor(colors.background)
        )
    }
}

/// Convert a libghostty-vt RGB triple into an NSColor in sRGB. The
/// C struct uses `uint8_t` per channel; NSColor wants 0..1 floats.
private func nsColor(_ c: GhosttyColorRgb) -> NSColor {
    NSColor(
        srgbRed: CGFloat(c.r) / 255.0,
        green: CGFloat(c.g) / 255.0,
        blue: CGFloat(c.b) / 255.0,
        alpha: 1.0
    )
}
