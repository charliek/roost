// AppKit view that hosts a libghostty-vt terminal.
//
// Phase 5 step 4a: own the libghostty-vt handle through the view's
// lifecycle, compute cell metrics from the chosen font, and draw a
// grid of empty cells with a faint border. This proves three things
// the rest of the renderer depends on:
//
//   * The Roost target itself can call libghostty-vt (not just the
//     RoostTests smoke). Pulling CGhosttyVT into App-side code
//     exercises the static-archive link path on the real binary.
//   * AppKit + Core Graphics can drive cell-aligned drawing at the
//     resolution we'll need for actual glyphs.
//   * The terminal handle survives view init / deinit cleanly with
//     the libghostty-vt FFI surface.
//
// Phase 5.4b will replace the placeholder grid with cells whose
// background colors come from walking the terminal's render state;
// 5.4c adds glyph rendering via Core Text; 5.5 wires the real PTY
// stream + keystrokes.

import AppKit
import CGhosttyVT

final class TerminalView: NSView {
    let cols: UInt16
    let rows: UInt16
    let cellSize: CGSize
    let font: NSFont

    /// libghostty-vt terminal handle. Held for lifecycle hygiene;
    /// Phase 5.4b starts using it to drive rendering.
    ///
    /// `nonisolated(unsafe)` because Swift 6 strict concurrency
    /// otherwise forbids the `@MainActor`-implicit NSView property
    /// from being touched in `deinit` (which is itself nonisolated).
    /// The promise the annotation makes is "no concurrent access" —
    /// safe here because the handle is allocated on the main thread
    /// in `init`, only ever referenced from main-thread `draw()` /
    /// future render-state walks, and freed on the main thread when
    /// the NSView is torn down. Revisit if any background-thread
    /// rendering path lands.
    nonisolated(unsafe) private var terminal: GhosttyTerminal?

    /// Pull-model snapshot of the terminal used by `draw()`.
    /// Phase 5.4b uses this only for the canvas color; 5.4c walks
    /// it cell-by-cell.
    private let renderState = RenderState()

    init(cols: UInt16 = 80, rows: UInt16 = 24) {
        self.cols = cols
        self.rows = rows

        // Cell metrics: monospaced system font, advance width measured
        // from a representative wide glyph ("M"), height from the
        // font's vertical metrics. Pinning to .monospacedSystemFont
        // means the metrics match what Core Text will draw later in
        // 5.4c so the cell grid + glyph layout stay aligned.
        let font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let cellWidth = NSAttributedString(
            string: "M",
            attributes: [.font: font]
        ).size().width.rounded(.up)
        let cellHeight = (font.ascender - font.descender + font.leading).rounded(.up)
        self.cellSize = CGSize(width: cellWidth, height: cellHeight)
        self.font = font

        super.init(
            frame: NSRect(
                x: 0,
                y: 0,
                width: cellWidth * CGFloat(cols),
                height: cellHeight * CGFloat(rows)
            )
        )

        // Construct the libghostty-vt terminal. Phase 5.4b will start
        // walking its render state to drive draw(); for now we just
        // hold the handle to validate the lifecycle on the real
        // (non-test) binary.
        var opts = GhosttyTerminalOptions()
        opts.cols = cols
        opts.rows = rows
        opts.max_scrollback = 0

        var handle: GhosttyTerminal?
        let rc = ghostty_terminal_new(nil, &handle, opts)
        if rc.rawValue != 0 || handle == nil {
            fatalError("ghostty_terminal_new failed (rc.rawValue=\(rc.rawValue))")
        }
        self.terminal = handle

        // Demo write so 5.4c is visibly different from 5.4b. Phase 5.5
        // replaces this with the real PTY stream from roost-core.
        // Sequence: red bg "ROOST", then green bg + blue fg "HI",
        // then default "  hello world".
        if let term = handle {
            let demo: [UInt8] = Array(
                "\u{001b}[41mROOST\u{001b}[0m\u{001b}[42m\u{001b}[34mHI\u{001b}[0m  hello world\r\n".utf8
            )
            demo.withUnsafeBufferPointer { ptr in
                ghostty_terminal_vt_write(term, ptr.baseAddress, demo.count)
            }
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("TerminalView is created programmatically; nib loading not supported")
    }

    deinit {
        if let term = terminal {
            ghostty_terminal_free(term)
        }
    }

    /// Use a top-left origin so cell (0, 0) is the upper-left corner —
    /// matches terminal coordinates and avoids one flip when 5.4b
    /// starts walking the render state.
    override var isFlipped: Bool { true }

    /// Lets AutoLayout size the view to its cell grid by default.
    /// Callers can still override with explicit constraints if they
    /// want a window-fit layout (the App's window does).
    override var intrinsicContentSize: NSSize {
        NSSize(
            width: cellSize.width * CGFloat(cols),
            height: cellSize.height * CGFloat(rows)
        )
    }

    override func draw(_ dirtyRect: NSRect) {
        // Snapshot terminal state, then ask libghostty-vt for the
        // current default fg/bg colors. The default-bg fill below
        // is the canvas the per-cell pass (Phase 5.4c) will paint
        // overrides on top of. Even with no shell attached yet, this
        // proves the render-state lifecycle round-trips: new -> update
        // -> colors_get -> free. The only visible change in this
        // commit vs 5.4a is the black-canvas swap to whatever the
        // terminal reports as its default bg (typically still black,
        // but no longer hardcoded).
        if let terminal {
            renderState.update(terminal: terminal)
        }
        let colors = renderState.defaultColors()
        colors.background.setFill()
        bounds.fill()

        // Per-cell content. Walk yields backgrounds (always
        // optional), grapheme characters (nil for empty cells),
        // and foregrounds (optional, fall back to default fg).
        // We do bg fills + glyph draws in a single pass so each
        // cell is touched only once.
        //
        // Glyph drawing currently uses NSAttributedString.draw —
        // simple, slow-but-correct. A glyph atlas (Core Text +
        // CGContextShowGlyphsAtPositions) is the next-tier
        // optimization once StreamPty starts pushing frames at
        // human-typing rates and per-cell allocations matter.
        let cellW = cellSize.width
        let cellH = cellSize.height
        let defaultFg = colors.foreground
        let cellFont = self.font
        renderState.walk { cell in
            let rect = NSRect(
                x: CGFloat(cell.col) * cellW,
                y: CGFloat(cell.row) * cellH,
                width: cellW,
                height: cellH
            )
            if let bg = cell.background {
                bg.setFill()
                rect.fill()
            }
            if let glyph = cell.glyph, !glyph.isWhitespace {
                let attrs: [NSAttributedString.Key: Any] = [
                    .font: cellFont,
                    .foregroundColor: cell.foreground ?? defaultFg,
                ]
                let line = NSAttributedString(string: String(glyph), attributes: attrs)
                // Bottom-align glyphs to the cell's baseline. The
                // grid origin is top-left (isFlipped=true), so the
                // glyph's drawing origin is at the cell top + the
                // font's ascender.
                line.draw(at: NSPoint(x: rect.minX, y: rect.minY))
            }
        }

        // Faint cell grid. 0.5pt offset on integer-pixel positions
        // gives crisp 1px-wide lines that don't anti-alias to
        // mush. Placeholder visual confirming the cell math; 5.4c
        // replaces this with per-cell content from the render state.
        NSColor(white: 0.15, alpha: 1.0).setStroke()
        let path = NSBezierPath()
        path.lineWidth = 0.5

        for col in 0...Int(cols) {
            let x = (CGFloat(col) * cellSize.width).rounded() + 0.5
            path.move(to: NSPoint(x: x, y: 0))
            path.line(to: NSPoint(x: x, y: bounds.height))
        }
        for row in 0...Int(rows) {
            let y = (CGFloat(row) * cellSize.height).rounded() + 0.5
            path.move(to: NSPoint(x: 0, y: y))
            path.line(to: NSPoint(x: bounds.width, y: y))
        }
        path.stroke()
    }
}
