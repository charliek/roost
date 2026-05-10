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

    /// Set by RoostApp so keystrokes captured here can be forwarded
    /// to the StreamPty writer. Phase 5.5b sends raw UTF-8 from
    /// `event.characters`; 5.5c upgrades to libghostty-vt's full
    /// key encoder for arrows / function keys / modifier handling.
    @MainActor var onKey: ((Data) -> Void)?

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
    }

    /// Feed VT bytes into the local libghostty-vt terminal and
    /// trigger a redraw. Called from the StreamPty consumer in
    /// `runShellSession` once per output chunk from `roost-core`.
    /// Must be called on the main actor — the terminal handle and
    /// AppKit invalidation both have main-thread requirements.
    @MainActor
    func appendBytes(_ data: Data) {
        guard let terminal else { return }
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            ghostty_terminal_vt_write(terminal, base, data.count)
        }
        needsDisplay = true
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

    /// Required for the view to receive `keyDown(with:)` events.
    /// RoostApp explicitly makes this view the window's first
    /// responder after construction so typing routes here.
    override var acceptsFirstResponder: Bool { true }

    /// Phase 5.5b: forward keystrokes to the StreamPty writer.
    ///
    /// Keys come in two flavors:
    ///
    ///   * **Printable / shell-canonical** — Tab, Enter, Backspace,
    ///     Ctrl+letter, etc. NSEvent.characters returns the right
    ///     bytes for these (Tab=`\t`, Enter=`\r`, Ctrl+C=`\u{0003}`),
    ///     so we just send them as UTF-8.
    ///
    ///   * **Special / function keys** — arrows, Home/End, Page
    ///     Up/Down, Delete (forward delete), Function keys.
    ///     NSEvent.characters returns NS-private codepoints in the
    ///     `\u{F700}+` range that shells don't recognize. The
    ///     standard fix is the libghostty-vt key encoder
    ///     (ghostty_key_encoder_*) which knows kitty keyboard modes,
    ///     modifier consumption, etc. For Phase 5.5c-lite we
    ///     translate the most common ones to plain VT/CSI escape
    ///     sequences directly. Full encoder integration is the next
    ///     bite (5.5c).
    override func keyDown(with event: NSEvent) {
        // Try the special-key table first; fall back to raw
        // NSEvent.characters for everything else.
        if let specialBytes = Self.specialKeyBytes(for: event) {
            onKey?(specialBytes)
            return
        }
        guard let chars = event.characters,
              let data = chars.data(using: .utf8),
              !data.isEmpty
        else {
            return
        }
        onKey?(data)
    }

    /// Translate well-known NS function-key codes to VT/CSI escape
    /// sequences. Returns nil for keys we don't recognize so the
    /// caller falls through to NSEvent.characters.
    ///
    /// Sequences chosen to match xterm's defaults — what virtually
    /// every shell + readline + tmux understands. No modifier
    /// support yet (Shift+Arrow, Option+Arrow); that lands with
    /// the libghostty-vt key encoder in Phase 5.5c.
    private static func specialKeyBytes(for event: NSEvent) -> Data? {
        guard let chars = event.charactersIgnoringModifiers,
              let scalar = chars.unicodeScalars.first
        else {
            return nil
        }
        // NS function-key constants are `Int`, scalar.value is `UInt32`.
        // Bridge through Int once for a clean switch.
        let codepoint = Int(scalar.value)
        let csi = Data([0x1b, 0x5b])  // ESC [
        let ss3 = Data([0x1b, 0x4f])  // ESC O (used for F1-F4)
        switch codepoint {
        // Arrows
        case NSUpArrowFunctionKey:    return csi + Data("A".utf8)
        case NSDownArrowFunctionKey:  return csi + Data("B".utf8)
        case NSRightArrowFunctionKey: return csi + Data("C".utf8)
        case NSLeftArrowFunctionKey:  return csi + Data("D".utf8)
        // Navigation
        case NSHomeFunctionKey:       return csi + Data("H".utf8)
        case NSEndFunctionKey:        return csi + Data("F".utf8)
        case NSPageUpFunctionKey:     return csi + Data("5~".utf8)
        case NSPageDownFunctionKey:   return csi + Data("6~".utf8)
        // Forward Delete (Fn+Delete on Mac). Backspace stays
        // NSEvent.characters (yields `\u{7f}` DEL — what most shells
        // expect for the backspace key on the main keyboard).
        case NSDeleteFunctionKey:     return csi + Data("3~".utf8)
        // Function keys F1-F4 use SS3, F5-F12 use CSI ~ encoding.
        case NSF1FunctionKey:  return ss3 + Data("P".utf8)
        case NSF2FunctionKey:  return ss3 + Data("Q".utf8)
        case NSF3FunctionKey:  return ss3 + Data("R".utf8)
        case NSF4FunctionKey:  return ss3 + Data("S".utf8)
        case NSF5FunctionKey:  return csi + Data("15~".utf8)
        case NSF6FunctionKey:  return csi + Data("17~".utf8)
        case NSF7FunctionKey:  return csi + Data("18~".utf8)
        case NSF8FunctionKey:  return csi + Data("19~".utf8)
        case NSF9FunctionKey:  return csi + Data("20~".utf8)
        case NSF10FunctionKey: return csi + Data("21~".utf8)
        case NSF11FunctionKey: return csi + Data("23~".utf8)
        case NSF12FunctionKey: return csi + Data("24~".utf8)
        default:
            return nil
        }
    }

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
