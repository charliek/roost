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

    /// One cell's renderable contents at frame snapshot time.
    /// `background` and `foreground` are nil when the cell defers
    /// to the terminal's default colors. `glyph` is nil when the
    /// cell has no graphemes (empty cell — possibly with a bg fill,
    /// e.g. erase-with-color).
    ///
    /// `bold` / `italic` / `inverse` mirror the SGR style bits the
    /// renderer needs to compute the effective fg/bg. `inverse`
    /// in particular drives the swap that makes codex's `\e[7m`
    /// gray prompt row render at all — without it the prompt
    /// disappears into the canvas background (the visible
    /// regression the PR fixes).
    struct Cell {
        let row: Int
        let col: Int
        let background: NSColor?
        let foreground: NSColor?
        let glyph: Character?
        let bold: Bool
        let italic: Bool
        let inverse: Bool
    }

    /// Walk every cell in the latest snapshot. The callback runs
    /// once per cell that the row iterator emits and must NOT call
    /// any other RenderState method — the iterators are reused
    /// per frame and re-entrancy would corrupt state.
    ///
    /// Phase 5.4d wires grapheme + foreground readback so glyphs
    /// render too. 5.4e will add styling (bold / italic / underline);
    /// for now those bits are dropped.
    func walk(_ fn: (Cell) -> Void) {
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

                // Background color: optional per cell. Default-bg
                // cells return a non-zero rc and we treat that as
                // "no override".
                var bg = GhosttyColorRgb()
                let bgRc = ghostty_render_state_row_cells_get(
                    cells,
                    GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
                    &bg
                )
                let background: NSColor? =
                    bgRc.rawValue == 0 ? nsColor(bg) : nil

                // Foreground color: same optional shape as bg.
                var fg = GhosttyColorRgb()
                let fgRc = ghostty_render_state_row_cells_get(
                    cells,
                    GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
                    &fg
                )
                let foreground: NSColor? =
                    fgRc.rawValue == 0 ? nsColor(fg) : nil

                // Grapheme codepoints: read length first, then the
                // buffer. Empty cells (graphLen == 0) emit no glyph
                // — the caller can still draw a bg fill over them.
                var graphLen: UInt32 = 0
                _ = ghostty_render_state_row_cells_get(
                    cells,
                    GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
                    &graphLen
                )

                var glyph: Character?
                if graphLen > 0 {
                    var cps = [UInt32](repeating: 0, count: Int(graphLen))
                    cps.withUnsafeMutableBufferPointer { ptr in
                        _ = ghostty_render_state_row_cells_get(
                            cells,
                            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                            UnsafeMutableRawPointer(ptr.baseAddress)
                        )
                    }
                    glyph = makeCharacter(from: cps)
                }

                // SGR style bits. GhosttyStyle is a sized C struct —
                // `.size` MUST be the struct size before the call so
                // libghostty knows which fields this caller is
                // prepared to read (forward-compat contract from
                // `ghostty/include/ghostty/vt/style.h`). Mirrors the
                // legacy Go FFI at `internal/ghostty/render.go:182-189`.
                // We treat any non-success rc as "no style data, use
                // defaults" — better to render the cell without
                // styling than to drop the whole frame.
                var style = GhosttyStyle()
                style.size = MemoryLayout<GhosttyStyle>.size
                let styleRc = ghostty_render_state_row_cells_get(
                    cells,
                    GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                    &style
                )
                let bold = styleRc.rawValue == 0 ? style.bold : false
                let italic = styleRc.rawValue == 0 ? style.italic : false
                let inverse = styleRc.rawValue == 0 ? style.inverse : false

                fn(Cell(
                    row: row,
                    col: col,
                    background: background,
                    foreground: foreground,
                    glyph: glyph,
                    bold: bold,
                    italic: italic,
                    inverse: inverse
                ))
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

    /// Cursor state for the current frame (goal-mac-polish-cursor-keys
    /// M2). `nil` means the cursor isn't in the visible viewport —
    /// don't draw it. All position values are viewport-relative
    /// (0-based row + col), respecting any active scroll-back.
    struct CursorInfo {
        let row: UInt16
        let col: UInt16
        /// True when the cursor lands on the second column of a
        /// wide (CJK / emoji) character. Renderers may choose to
        /// shift / underline differently in this case.
        let wideTail: Bool
        /// DECTCEM (terminal mode 25) state — `false` means the
        /// terminal has explicitly hidden its cursor. Renderers
        /// MUST honor this; otherwise apps like `less` show a
        /// stray cursor.
        let visible: Bool
        /// Whether DECSCUSR set the cursor to a blinking style.
        /// Roost's blink timer applies regardless, but apps may
        /// flip this to request "always solid" — when false,
        /// renderers should freeze the cursor on.
        let blinking: Bool
        let visualStyle: GhosttyRenderStateCursorVisualStyle
        /// Explicit cursor color (OSC 12), or `nil` when the
        /// terminal hasn't overridden the theme's cursor color.
        let color: NSColor?
    }

    /// Read the current cursor state from the latest snapshot.
    /// Returns `nil` when the cursor isn't in the visible viewport
    /// (user has scrolled back past the cursor row); callers
    /// shouldn't draw a cursor in that case.
    func cursor() -> CursorInfo? {
        guard let rs else { return nil }

        var hasValue: Bool = false
        _ = ghostty_render_state_get(
            rs,
            GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE,
            &hasValue
        )
        guard hasValue else { return nil }

        var x: UInt16 = 0
        var y: UInt16 = 0
        var wideTail: Bool = false
        var visible: Bool = false
        var blinking: Bool = false
        var style: GhosttyRenderStateCursorVisualStyle = GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK
        _ = ghostty_render_state_get(rs, GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X, &x)
        _ = ghostty_render_state_get(rs, GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y, &y)
        _ = ghostty_render_state_get(rs, GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_WIDE_TAIL, &wideTail)
        _ = ghostty_render_state_get(rs, GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE, &visible)
        _ = ghostty_render_state_get(rs, GHOSTTY_RENDER_STATE_DATA_CURSOR_BLINKING, &blinking)
        _ = ghostty_render_state_get(rs, GHOSTTY_RENDER_STATE_DATA_CURSOR_VISUAL_STYLE, &style)

        // Explicit cursor color is set via OSC 12; absent otherwise.
        // The HAS_VALUE gate prevents reading uninitialized RGB bytes.
        var colorHasValue: Bool = false
        _ = ghostty_render_state_get(
            rs,
            GHOSTTY_RENDER_STATE_DATA_COLOR_CURSOR_HAS_VALUE,
            &colorHasValue
        )
        var color: NSColor?
        if colorHasValue {
            var rgb = GhosttyColorRgb()
            let rc = ghostty_render_state_get(
                rs,
                GHOSTTY_RENDER_STATE_DATA_COLOR_CURSOR,
                &rgb
            )
            if rc.rawValue == 0 {
                color = nsColor(rgb)
            }
        }

        return CursorInfo(
            row: y,
            col: x,
            wideTail: wideTail,
            visible: visible,
            blinking: blinking,
            visualStyle: style,
            color: color
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

/// Build a `Character` from a libghostty-vt grapheme codepoint
/// sequence. Most cells have len == 1 (ASCII / single Unicode
/// scalar). Multi-codepoint clusters (combining marks, ZWJ emoji)
/// concatenate into one Character via Swift's String grapheme
/// breaker. Returns nil if the sequence has no valid scalars.
private func makeCharacter(from codepoints: [UInt32]) -> Character? {
    var s = String()
    s.reserveCapacity(codepoints.count)
    for cp in codepoints {
        guard let scalar = Unicode.Scalar(cp) else { continue }
        s.unicodeScalars.append(scalar)
    }
    return s.first
}
