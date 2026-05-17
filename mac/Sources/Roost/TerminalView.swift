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
    /// Live cell metrics. `cellSize` recomputes when `font` changes
    /// via `updateFont(_:)` (Phase 6a P2 font_increase / decrease /
    /// reset). Both stay `private(set)` because callers shouldn't
    /// poke them directly — the update path keeps libghostty-vt's
    /// cell-grid in sync with the AppKit cell metrics.
    private(set) var cellSize: CGSize
    private(set) var font: NSFont

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

    /// libghostty-vt key encoder bridge (goal-mac-polish-cursor-keys
    /// M1). Allocated alongside the terminal handle in `init`, freed
    /// implicitly before the terminal in `deinit`. Optional only so
    /// init can fall through gracefully if the encoder allocation
    /// fails — in practice that should never happen.
    private var keyEncoder: KeyEncoder?

    /// Pull-model snapshot of the terminal used by `draw()`.
    /// Phase 5.4b uses this only for the canvas color; 5.4c walks
    /// it cell-by-cell.
    private let renderState = RenderState()

    /// Set by RoostApp so keystrokes captured here can be forwarded
    /// to the StreamPty writer. Phase 5.5b sends raw UTF-8 from
    /// `event.characters`; 5.5c upgrades to libghostty-vt's full
    /// key encoder for arrows / function keys / modifier handling.
    @MainActor var onKey: ((Data) -> Void)?

    /// Set by `TabSession.start` so OSC events parsed out of the
    /// PTY byte stream (Phase 6a P6) ride the existing ReportOsc
    /// gRPC path to the daemon. First arg is the OSC command
    /// number; second is the payload in the shape
    /// `RoostService::report_osc` expects (see `OscEvent.asReport`
    /// for the mapping).
    @MainActor var onOsc: ((UInt32, String) -> Void)?

    /// Local OSC scanner — observes every PTY-output chunk
    /// `appendBytes` writes through to libghostty so we can lift
    /// title / cwd / notification OSCs to the daemon. State
    /// persists across calls so sequences split across chunks
    /// scan correctly.
    private let oscScanner = OscScanner()

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

        // goal-mac-polish-cursor-keys M1: real key encoder bridge.
        // Replaces the hand-rolled `specialKeyBytes` table that used
        // to live in `keyDown` — fixes Shift+Tab, Shift+Enter,
        // Option+Arrow, Ctrl+letter, and so on by routing every
        // NSEvent through libghostty-vt's `ghostty_key_encoder_*`
        // surface (same one the Go binary uses via
        // `internal/ghostty/key.go`).
        self.keyEncoder = KeyEncoder(terminal: handle!)

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
        // Phase 6a P6: scan the chunk for OSC events BEFORE
        // feeding it to libghostty. libghostty's own VT processor
        // also handles OSCs (titles, colors, etc.) for rendering,
        // but it doesn't surface the events back out for the
        // daemon to route. We run a parallel scanner so the
        // daemon can react to title / cwd / notification OSCs.
        // The bytes still flow through to libghostty unchanged —
        // the scanner is purely additive.
        let events = oscScanner.feed(data)
        for event in events {
            let (cmd, payload) = event.asReport
            onOsc?(cmd, payload)
        }
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
    /// Every keystroke routes through `KeyEncoder.encode` (the M1
    /// libghostty-vt bridge). The encoder respects live terminal
    /// state — cursor-key application mode, Kitty keyboard flags,
    /// modifyOtherKeys, etc. — and produces the correct escape
    /// sequence for Shift+Tab (`\x1b[Z`), Shift+Enter, Option+Arrow,
    /// Ctrl+letter, and arrow / function / navigation keys. Same
    /// surface the Go binary uses via `internal/ghostty/key.go`.
    override func keyDown(with event: NSEvent) {
        guard let keyEncoder else { return }
        let bytes = keyEncoder.encode(event)
        // Empty bytes mean the encoder swallowed the event
        // (modifier-only press, IME dead-key, etc.) — don't propagate
        // a zero-length write to the PTY.
        guard !bytes.isEmpty else { return }
        onKey?(bytes)
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

    /// Swap the active font (Phase 6a P2 font_increase / decrease /
    /// reset). Recomputes `cellSize` from the new font's advance
    /// width + line height, then re-runs the standard reflow path
    /// so the libghostty cell grid + PTY winsize converge on the
    /// new metrics. AppKit's intrinsic-content-size + setFrameSize
    /// loop picks up the rest.
    @MainActor
    func updateFont(_ newFont: NSFont) {
        // No-op when the font didn't actually change — caller may
        // clamp font size into a saturating range and call us with
        // the same NSFont.
        if newFont.fontName == self.font.fontName && newFont.pointSize == self.font.pointSize {
            return
        }
        self.font = newFont
        let cellWidth = NSAttributedString(
            string: "M",
            attributes: [.font: newFont]
        ).size().width.rounded(.up)
        let cellHeight = (newFont.ascender - newFont.descender + newFont.leading).rounded(.up)
        self.cellSize = CGSize(width: cellWidth, height: cellHeight)
        invalidateIntrinsicContentSize()
        // Force a reflow against the current bounds with the NEW
        // cell metrics. `reflowGridForBounds` short-circuits when
        // cols/rows are unchanged — since cellSize just changed
        // the cell count almost certainly differs, but we also
        // need to push the new pixel cell size to libghostty so
        // its graphics-extension consumers see the right per-cell
        // dimensions.
        reflowGridForBounds(forceResize: true)
        needsDisplay = true
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
    private func reflowGridForBounds(forceResize: Bool = false) {
        guard cellSize.width > 0, cellSize.height > 0 else { return }
        let newCols = max(1, UInt16(floor(bounds.width / cellSize.width)))
        let newRows = max(1, UInt16(floor(bounds.height / cellSize.height)))
        let dimsChanged = (newCols != cols) || (newRows != rows)
        if !dimsChanged && !forceResize { return }
        cols = newCols
        rows = newRows
        if let terminal {
            let cellWPx = UInt32(cellSize.width.rounded())
            let cellHPx = UInt32(cellSize.height.rounded())
            _ = ghostty_terminal_resize(terminal, newCols, newRows, cellWPx, cellHPx)
        }
        invalidateIntrinsicContentSize()
        needsDisplay = true
        if dimsChanged {
            onResize?(newCols, newRows)
        }
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
