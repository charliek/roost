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
    /// Live cell-grid metrics. `cols` and `rows` follow the view's
    /// bounds — Phase 6a M3 lifts the previous "fixed at init"
    /// invariant so window resize reflows the terminal.
    private(set) var cols: UInt16
    private(set) var rows: UInt16
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

    /// Set by `TabSession.start` so the view can ask its host to
    /// propagate a resize over StreamPty when the window resizes
    /// and the cell grid changes shape.
    @MainActor var onResize: ((UInt16, UInt16) -> Void)?

    // MARK: - Selection state (Phase 6a M5)

    /// Cell-coordinate selection state. Two cells form the
    /// rectangle, normalized at draw + extract time so the user can
    /// drag in any direction. Both points are inclusive on the
    /// start side and exclusive on the end side — matching the
    /// "drag-from-cursor" convention every other terminal uses.
    private struct CellSelection {
        var anchorCol: Int
        var anchorRow: Int
        var cursorCol: Int
        var cursorRow: Int

        /// True if the anchor and cursor land on the same cell —
        /// shouldn't render selection chrome for a single-cell
        /// "click but didn't drag" gesture.
        var isEmpty: Bool { anchorCol == cursorCol && anchorRow == cursorRow }

        /// Inclusive (startRow, startCol) and exclusive
        /// (endRow, endCol) in the normalized direction.
        func normalized() -> (startRow: Int, startCol: Int, endRow: Int, endCol: Int) {
            let (sRow, sCol, eRow, eCol): (Int, Int, Int, Int)
            if anchorRow == cursorRow {
                sRow = anchorRow
                eRow = anchorRow + 1
                sCol = min(anchorCol, cursorCol)
                eCol = max(anchorCol, cursorCol) + 1
            } else if anchorRow < cursorRow {
                sRow = anchorRow
                sCol = anchorCol
                eRow = cursorRow + 1
                eCol = cursorCol + 1
            } else {
                sRow = cursorRow
                sCol = cursorCol
                eRow = anchorRow + 1
                eCol = anchorCol + 1
            }
            return (sRow, sCol, eRow, eCol)
        }
    }

    private var selection: CellSelection?

    /// Active theme. M6 first-cut: applied to the canvas + selection
    /// overlay + glyph fallback colors. Loaded from config on launch;
    /// the runtime fallback is the bundled `roost-dark` theme.
    private var theme: Theme = .fallback

    init(
        cols: UInt16 = 80,
        rows: UInt16 = 24,
        theme: Theme = .fallback,
        font: NSFont = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
    ) {
        self.cols = cols
        self.rows = rows
        self.theme = theme

        // Cell metrics: monospaced system font, advance width measured
        // from a representative wide glyph ("M"), height from the
        // font's vertical metrics. Caller can override via the `font`
        // arg so config-driven font-size (Phase 6a M6) flows through.
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

        // Phase 6a P3: push the theme's fg / bg / cursor + 256-color
        // palette into libghostty-vt so SGR-color cells render in the
        // theme's palette instead of libghostty's compiled-in default.
        // M6 only changed the canvas + selection colors at draw time;
        // this closes the SGR-cell gap so `ls --color`, `git diff`,
        // `htop` etc. all flip with the active theme. Mirrors the
        // Go binary's `internal/ghostty/terminal.go::SetTheme`. MUST
        // run before any `ghostty_terminal_vt_write` so the very
        // first frame paints with the right colors.
        Theme.apply(theme, to: handle!)

        // Let edge-pinned hosts (the `terminalContainer` in
        // `RoostApp.selectTab`) stretch the view past its 80×24
        // intrinsic size. Without this AutoLayout would honor the
        // intrinsic content size and leave dead space inside the
        // container.
        setContentHuggingPriority(.defaultLow - 1, for: .horizontal)
        setContentHuggingPriority(.defaultLow - 1, for: .vertical)
        setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        setContentCompressionResistancePriority(.defaultLow, for: .vertical)
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

    // MARK: - Selection (Phase 6a M5)

    /// Convert a point in view coordinates to (col, row) clamped to
    /// the cell grid. `isFlipped` is true so `y=0` is the top edge —
    /// no flip needed.
    private func cellAt(point: NSPoint) -> (col: Int, row: Int) {
        guard cellSize.width > 0, cellSize.height > 0 else { return (0, 0) }
        let col = max(0, min(Int(cols) - 1, Int(point.x / cellSize.width)))
        let row = max(0, min(Int(rows) - 1, Int(point.y / cellSize.height)))
        return (col, row)
    }

    override func mouseDown(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        let cell = cellAt(point: p)
        selection = CellSelection(
            anchorCol: cell.col,
            anchorRow: cell.row,
            cursorCol: cell.col,
            cursorRow: cell.row
        )
        needsDisplay = true
    }

    override func mouseDragged(with event: NSEvent) {
        guard var sel = selection else { return }
        let p = convert(event.locationInWindow, from: nil)
        let cell = cellAt(point: p)
        sel.cursorCol = cell.col
        sel.cursorRow = cell.row
        selection = sel
        needsDisplay = true
    }

    override func mouseUp(with event: NSEvent) {
        // If the drag never moved (anchor == cursor), clear the
        // selection state — a single-cell "click but didn't drag"
        // shouldn't leave a stray highlight. Real selections persist
        // until the next mouseDown or `clearSelection()`.
        if let sel = selection, sel.isEmpty {
            selection = nil
            needsDisplay = true
        }
    }

    /// Reset the selection. Hooked by `RoostApp` when switching
    /// tabs / projects so a selection from one tab doesn't bleed
    /// into another.
    @MainActor
    func clearSelection() {
        if selection != nil {
            selection = nil
            needsDisplay = true
        }
    }

    /// Standard responder-chain copy. `⌘C` is wired into the App's
    /// Edit menu via `selector: #selector(NSText.copy(_:))`; AppKit
    /// routes it here once the TerminalView is first responder.
    /// Walks the render-state snapshot for the selected cell rect,
    /// joins glyphs into rows, and writes plain text to the
    /// general pasteboard. No-op when there's no selection.
    @objc
    func copy(_ sender: Any?) {
        guard let text = selectedPlainText(), !text.isEmpty else { return }
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(text, forType: .string)
    }

    /// Standard responder-chain paste. Reads the pasteboard's
    /// string contents, asks libghostty-vt whether the shell has
    /// enabled bracketed-paste mode (DECSET 2004), and wraps the
    /// payload in `ESC[200~ … ESC[201~` if so. Falls through to
    /// raw bytes when the shell hasn't asked for bracketed paste
    /// (e.g. `cat`, basic POSIX shells).
    @objc
    func paste(_ sender: Any?) {
        guard let s = NSPasteboard.general.string(forType: .string),
              !s.isEmpty
        else { return }
        var payload = Data(s.utf8)
        if bracketedPasteEnabled() {
            // ESC [ 2 0 0 ~ … ESC [ 2 0 1 ~
            var wrapped = Data([0x1b, 0x5b, 0x32, 0x30, 0x30, 0x7e])
            wrapped.append(payload)
            wrapped.append(contentsOf: [0x1b, 0x5b, 0x32, 0x30, 0x31, 0x7e])
            payload = wrapped
        }
        onKey?(payload)
    }

    /// Walk the latest render-state snapshot and concatenate the
    /// glyphs inside the current selection. Trims trailing whitespace
    /// per row + drops empty trailing rows so a multi-line copy
    /// doesn't carry a wall of spaces from cells the terminal hasn't
    /// drawn into. Returns nil when there's no selection.
    @MainActor
    private func selectedPlainText() -> String? {
        guard let sel = selection else { return nil }
        let n = sel.normalized()
        if let terminal { renderState.update(terminal: terminal) }
        var rows: [String] = []
        for _ in n.startRow..<n.endRow { rows.append("") }
        renderState.walk { cell in
            guard cell.row >= n.startRow, cell.row < n.endRow else { return }
            guard cell.col >= n.startCol, cell.col < n.endCol else { return }
            let idx = cell.row - n.startRow
            if let g = cell.glyph {
                rows[idx].append(String(g))
            } else {
                rows[idx].append(" ")
            }
        }
        var trimmed = rows.map {
            String($0.reversed().drop(while: { $0 == " " }).reversed())
        }
        while let last = trimmed.last, last.isEmpty {
            trimmed.removeLast()
        }
        return trimmed.joined(separator: "\n")
    }

    /// Ask libghostty-vt whether the shell has enabled bracketed-paste
    /// mode. Defaults to `false` on any FFI hiccup so we don't wrap a
    /// paste in escape sequences a confused shell would echo back at
    /// the user.
    ///
    /// `GHOSTTY_MODE_BRACKETED_PASTE` is a function-call macro in the
    /// C header (`ghostty_mode_new(2004, false)`), which the Swift
    /// importer can't bridge — Swift drops macros whose body isn't a
    /// plain constant. Reconstruct the mode value inline using the
    /// same bit packing as the `ghostty_mode_new` helper:
    ///     low 15 bits = the mode number (2004),
    ///     bit 15      = the ANSI flag (false for DEC private modes).
    @MainActor
    private func bracketedPasteEnabled() -> Bool {
        guard let terminal else { return false }
        var on = false
        let mode = GhosttyMode(2004 & 0x7FFF)
        let rc = ghostty_terminal_mode_get(terminal, mode, &on)
        return rc.rawValue == 0 && on
    }

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

    /// On every frame size change, reflow the cell grid. Floor-
    /// quantizing the available pixels by `cellSize` keeps the
    /// rendered grid pixel-aligned — anything else and we'd get
    /// fractional-pixel cell boundaries that smear glyphs on a
    /// HiDPI display.
    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        reflowGridForBounds()
    }

    /// Compute the largest (cols, rows) that fit in the current
    /// bounds, push them to libghostty-vt via
    /// `ghostty_terminal_resize`, fire `onResize` so the host can
    /// propagate over StreamPty, and request a redraw.
    ///
    /// `cell_width_px` / `cell_height_px` are passed as integer
    /// pixel sizes — libghostty-vt's resize signature exposes them
    /// for graphics-extension consumers (e.g. sixel / Kitty
    /// graphics). We round to nearest because the rendered cells
    /// use the same `cellSize.width`-typed integer step in
    /// `draw(_:)`.
    @MainActor
    private func reflowGridForBounds() {
        guard cellSize.width > 0, cellSize.height > 0 else { return }
        let newCols = max(1, UInt16(floor(bounds.width / cellSize.width)))
        let newRows = max(1, UInt16(floor(bounds.height / cellSize.height)))
        if newCols == cols && newRows == rows { return }
        cols = newCols
        rows = newRows
        if let terminal {
            let cellWPx = UInt32(cellSize.width.rounded())
            let cellHPx = UInt32(cellSize.height.rounded())
            _ = ghostty_terminal_resize(terminal, newCols, newRows, cellWPx, cellHPx)
        }
        invalidateIntrinsicContentSize()
        needsDisplay = true
        onResize?(newCols, newRows)
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
        // Phase 6a M6: prefer the user-loaded theme's fg/bg over
        // libghostty-vt's compiled-in defaults. libghostty's
        // default colors are what the embedded shell sees as
        // `colors().background` when it queries via VT — flipping
        // them at the libghostty level would require a `_set` call
        // per startup, which the per-tab spawn lifecycle makes
        // fiddly. Painting the canvas with the theme color here is
        // visually equivalent for cells whose bg defers to the
        // default; cells with explicit bg overrides still hit the
        // walk path below.
        let libDefaults = renderState.defaultColors()
        let canvasBg = theme.background
        let canvasFg = theme.foreground
        canvasBg.setFill()
        bounds.fill()
        _ = libDefaults

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
        let defaultFg = canvasFg
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

        // Selection overlay (Phase 6a M5). Drawn last so it sits on
        // top of the glyph pass — translucent accent fill, no border.
        if let sel = selection, !sel.isEmpty {
            let n = sel.normalized()
            let overlay = theme.selectionBackground.withAlphaComponent(0.6)
            overlay.setFill()
            for row in n.startRow..<n.endRow {
                // Row-aware horizontal extent: a single-row selection
                // spans the user's start/end cols; a multi-row
                // selection spans full rows for every interior row.
                let isFirst = (row == n.startRow)
                let isLast = (row == n.endRow - 1)
                let startCol: Int
                let endCol: Int
                if n.endRow - n.startRow == 1 {
                    startCol = n.startCol
                    endCol = n.endCol
                } else if isFirst {
                    startCol = n.startCol
                    endCol = Int(cols)
                } else if isLast {
                    startCol = 0
                    endCol = n.endCol
                } else {
                    startCol = 0
                    endCol = Int(cols)
                }
                let r = NSRect(
                    x: CGFloat(startCol) * cellW,
                    y: CGFloat(row) * cellH,
                    width: CGFloat(endCol - startCol) * cellW,
                    height: cellH
                )
                r.fill()
            }
        }
    }
}
