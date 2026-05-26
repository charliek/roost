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
    /// Maximum scrollback rows libghostty-vt retains per terminal.
    /// Matches Go binary's `cmd/roost/session.go:186` (M6).
    static let defaultScrollback: size_t = 2000

    /// Discrete scroll-wheel notches → terminal rows. Matches Go
    /// `cmd/roost/session.go:794`: ~3 rows per click on a discrete
    /// wheel. Trackpad smooth-scroll bypasses this multiplier and
    /// uses point-precision accumulation instead.
    private static let rowsPerWheelNotch: Int = 3

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

    /// libghostty-vt mouse encoder bridge. Allocated alongside the
    /// terminal + key encoder in `init`, freed implicitly in `deinit`.
    /// Used by `scrollWheel` to forward the wheel as button-4/5 reports
    /// when the focused app enables mouse tracking.
    private var mouseEncoder: MouseEncoder?

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

    // MARK: - Cursor blink (goal-mac-polish-cursor-keys M2)

    /// Toggled by `cursorBlinkTimer` every `cursorBlinkPeriod`. `true`
    /// means draw the cursor this frame; `false` means skip (the cell
    /// underneath shows through). Forced back to `true` whenever
    /// focus is regained so the cursor reappears immediately instead
    /// of waiting for the next blink tick.
    private var cursorBlinkOn: Bool = true

    /// 530ms cadence matches the Go binary's `cursorBlinkPeriod`
    /// (`cmd/roost/session.go:37`), which is also iTerm2 / Terminal.app's
    /// default. Paused while the view doesn't have focus; restarted
    /// on focus regain.
    ///
    /// `nonisolated(unsafe)`: Swift 6 strict concurrency otherwise
    /// rejects `cursorBlinkTimer?.invalidate()` in the nonisolated
    /// `deinit`. The timer is only touched from the main thread —
    /// scheduled in `startCursorBlink`, invalidated here + in
    /// `stopCursorBlink`. Same shape as the other unsafe FFI handles
    /// in this file.
    nonisolated(unsafe) private var cursorBlinkTimer: Timer?

    /// Last-known window-key state, updated by NSNotification observers
    /// in `viewDidMoveToWindow`.
    private var windowIsKey: Bool = false

    /// First-responder tracking. Updated in
    /// `becomeFirstResponder` / `resignFirstResponder`. We combine
    /// this with `windowIsKey` to derive "the user is actively typing
    /// into this terminal."
    private var viewIsFirstResponder: Bool = false

    /// True when the cursor should render as a focused (solid block /
    /// bar / underline depending on style) versus a blurred hollow
    /// outline. Mirrors Go `cmd/roost/session.go::windowFocused`.
    private var hasFocus: Bool { windowIsKey && viewIsFirstResponder }

    /// Cached glyph from the cell the cursor lands on. Stashed during
    /// the per-cell walk in `draw(_:)`; consumed by the cursor-draw
    /// pass at end-of-frame so a focused block cursor can re-paint
    /// the glyph in an inverted color (cursor block paints OVER the
    /// original glyph, so we have to redraw it to keep the character
    /// visible).
    private var cursorCellGlyph: Character?
    private var cursorCellOriginalForeground: NSColor?

    // MARK: - Scrollback (goal-mac-polish-cursor-keys M6)

    /// Fractional accumulator for trackpad smooth-scroll. NSEvent's
    /// `scrollingDeltaY` is in points (continuous on Magic Mouse /
    /// trackpads). We translate to rows by dividing by `cellSize.height`,
    /// accumulate the fractional remainder, dispatch whole rows when
    /// `|accum|` crosses a row boundary. Reset on direction change so
    /// the user can quickly flick back without overshoot.
    private var scrollAccum: Double = 0.0
    private var lastScrollDirection: Int = 0

    /// `true` when the viewport has been scrolled away from the bottom
    /// by a local-scroll event. Cleared by the snap-to-bottom hook in
    /// `keyDown`, which fires `GHOSTTY_SCROLL_VIEWPORT_BOTTOM` before
    /// the keystroke reaches libghostty. Mirrors Go
    /// `cmd/roost/input.go:67` ("Snap viewport before delivering the
    /// keystroke") + `cmd/roost/session.go:108-112` (`scrolledBack`).
    private var scrolledBack: Bool = false

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
        // M6: enable scrollback storage. Matches Go binary's
        // MaxScrollback: 2000 in cmd/roost/session.go:186. Without
        // a positive value libghostty-vt discards lines as they
        // scroll off the screen — scroll-wheel events would have
        // nothing to scroll into.
        opts.max_scrollback = Self.defaultScrollback

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
        // Go binary's `internal/ghostty/terminal.go::SetTheme`. Done
        // here so the very first frame paints with the right colors;
        // `setTheme(_:)` re-applies the same way for live theme swaps.
        Theme.apply(theme, to: handle!)

        // goal-mac-polish-cursor-keys M1: real key encoder bridge.
        // Replaces the hand-rolled `specialKeyBytes` table that used
        // to live in `keyDown` — fixes Shift+Tab, Shift+Enter,
        // Option+Arrow, Ctrl+letter, and so on by routing every
        // NSEvent through libghostty-vt's `ghostty_key_encoder_*`
        // surface (same one the Go binary uses via
        // `internal/ghostty/key.go`).
        self.keyEncoder = KeyEncoder(terminal: handle!)
        self.mouseEncoder = MouseEncoder(terminal: handle!)

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

    /// Swap the active theme on a live terminal. Repaints the canvas /
    /// selection / cursor (which read `self.theme` directly in
    /// `draw(_:)`) and re-applies the fg/bg/cursor + palette into
    /// libghostty-vt so SGR-indexed cells recolor too. Safe mid-session
    /// (see `themeAppliesAfterVtWrite`). The two-arg form lets a caller
    /// reuse a pre-`resolved()` palette across many terminals (the live
    /// preview broadcasts to every open tab on each keypress).
    @MainActor
    func setTheme(_ theme: Theme) {
        setTheme(theme, resolved: theme.resolved())
    }

    @MainActor
    func setTheme(_ theme: Theme, resolved: Theme.Resolved) {
        self.theme = theme
        if let terminal {
            Theme.apply(resolved, to: terminal)
        }
        needsDisplay = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("TerminalView is created programmatically; nib loading not supported")
    }

    deinit {
        // Drop cursor blink timer + notification observers BEFORE
        // freeing the unsafe FFI handle — both touch state from
        // the same actor and order matters for clean teardown.
        NotificationCenter.default.removeObserver(self)
        cursorBlinkTimer?.invalidate()
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

    // MARK: - Focus + cursor blink lifecycle

    /// Track window-key state so the blink timer can pause when the
    /// user clicks away. Registers / deregisters for `didBecomeKey`
    /// / `didResignKey` notifications keyed by the view's window —
    /// the view can be reparented (Phase 6a M3 tab switch) so we
    /// re-attach on each move.
    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        let center = NotificationCenter.default
        center.removeObserver(self)
        guard let window else {
            windowIsKey = false
            stopCursorBlink()
            return
        }
        center.addObserver(
            self,
            selector: #selector(handleWindowDidBecomeKey(_:)),
            name: NSWindow.didBecomeKeyNotification,
            object: window
        )
        center.addObserver(
            self,
            selector: #selector(handleWindowDidResignKey(_:)),
            name: NSWindow.didResignKeyNotification,
            object: window
        )
        windowIsKey = window.isKeyWindow
        updateBlinkTimerForFocus()
    }

    @objc private func handleWindowDidBecomeKey(_ note: Notification) {
        windowIsKey = true
        // Snap the cursor on so it appears immediately rather than at
        // the next blink tick — matches Go session.go:1232 behavior.
        cursorBlinkOn = true
        updateBlinkTimerForFocus()
        needsDisplay = true
    }

    @objc private func handleWindowDidResignKey(_ note: Notification) {
        windowIsKey = false
        updateBlinkTimerForFocus()
        needsDisplay = true
    }

    override func becomeFirstResponder() -> Bool {
        let became = super.becomeFirstResponder()
        if became {
            viewIsFirstResponder = true
            cursorBlinkOn = true
            updateBlinkTimerForFocus()
            needsDisplay = true
        }
        return became
    }

    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned {
            viewIsFirstResponder = false
            updateBlinkTimerForFocus()
            needsDisplay = true
        }
        return resigned
    }

    /// Drive the blink timer based on current focus. The timer only
    /// runs while the cursor is genuinely interactive — window is
    /// key + view is first responder. Outside that window we render
    /// the cursor as a hollow outline that's always on, so the timer
    /// would be wasted work.
    private func updateBlinkTimerForFocus() {
        if hasFocus {
            startCursorBlink()
        } else {
            stopCursorBlink()
        }
    }

    private func startCursorBlink() {
        guard cursorBlinkTimer == nil else { return }
        // 530ms half-period (= ~0.94 Hz full blink cycle). Common
        // terminal value; matches Go `cursorBlinkPeriod` in
        // `cmd/roost/session.go:37`.
        cursorBlinkTimer = Timer.scheduledTimer(
            withTimeInterval: 0.530,
            repeats: true
        ) { [weak self] _ in
            Task { @MainActor [weak self] in
                guard let self else { return }
                self.cursorBlinkOn.toggle()
                self.needsDisplay = true
            }
        }
    }

    private func stopCursorBlink() {
        cursorBlinkTimer?.invalidate()
        cursorBlinkTimer = nil
    }

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

    /// Text snapshot of the full viewport for the `tab.dump` IPC op.
    struct Dump {
        struct Cursor {
            let row: Int
            let col: Int
            let visible: Bool
        }
        let cols: Int
        let rows: Int
        let cursor: Cursor?
        let rowsText: [String]
    }

    /// Snapshot the whole viewport as text for `tab.dump`: one rstripped
    /// line per row (a blank cell becomes a space so columns line up)
    /// plus the cursor. Mirrors `selectedPlainText` / `draw`'s
    /// update→walk, but over every row. Main-thread-only — touches the
    /// libghostty handle + render state.
    @MainActor
    func dumpText() -> Dump {
        if let terminal { renderState.update(terminal: terminal) }
        let cursorInfo = renderState.cursor()
        var lines = [String](repeating: "", count: Int(rows))
        renderState.walk { cell in
            guard cell.row >= 0, cell.row < lines.count else { return }
            if let g = cell.glyph {
                lines[cell.row].append(String(g))
            } else {
                lines[cell.row].append(" ")
            }
        }
        let trimmed = lines.map {
            String($0.reversed().drop(while: { $0 == " " }).reversed())
        }
        let cursor = cursorInfo.map {
            Dump.Cursor(row: Int($0.row), col: Int($0.col), visible: $0.visible)
        }
        return Dump(cols: Int(cols), rows: Int(rows), cursor: cursor, rowsText: trimmed)
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
        // M6: snap the viewport back to the bottom before delivering
        // the keystroke. Mirrors Go input.go:67. Without this, typing
        // while scrolled-back would let the shell prompt scroll off
        // the visible area on the next render.
        if scrolledBack, let terminal {
            var sv = GhosttyTerminalScrollViewport()
            sv.tag = GHOSTTY_SCROLL_VIEWPORT_BOTTOM
            ghostty_terminal_scroll_viewport(terminal, sv)
            scrolledBack = false
            scrollAccum = 0
            lastScrollDirection = 0
            needsDisplay = true
        }
        let bytes = keyEncoder.encode(event)
        // Empty bytes mean the encoder swallowed the event
        // (modifier-only press, IME dead-key, etc.) — don't propagate
        // a zero-length write to the PTY.
        guard !bytes.isEmpty else { return }
        onKey?(bytes)
    }

    /// Wheel / trackpad scroll. Three behaviors depending on terminal
    /// state, matching Go `cmd/roost/session.go::handleScroll`
    /// (:776-900):
    ///
    ///   1. Mouse-tracking mode → mouse-button-4/5 encode through
    ///      a libghostty-vt mouse encoder. Deferred to a follow-up
    ///      `polish/mouse-tracking-encoder` branch — for now we
    ///      silently drop the event so the alt-screen / local
    ///      paths stay simple.
    ///   2. Alt-screen, no mouse tracking → translate to Up/Down
    ///      arrow key presses through the existing key encoder so
    ///      vim / less behave like the user expects.
    ///   3. Primary screen, no mouse tracking → local viewport
    ///      scroll via `ghostty_terminal_scroll_viewport`. Sets
    ///      `scrolledBack` so the next keystroke snaps to bottom.
    override func scrollWheel(with event: NSEvent) {
        guard let terminal else { return }
        let rowDelta = quantizeScrollDelta(event: event)
        guard rowDelta != 0 else { return }

        // Mouse tracking mode: the app opted into mouse events, so
        // forward the wheel as button-4 (up) / button-5 (down) reports
        // at the pointer's cell. One report per quantized row. The
        // encoder honors the negotiated format (X10 / SGR / pixels).
        if isMouseTrackingActive() {
            guard let mouseEncoder else { return }
            let button: GhosttyMouseButton =
                rowDelta > 0 ? GHOSTTY_MOUSE_BUTTON_FOUR : GHOSTTY_MOUSE_BUTTON_FIVE
            let mods = Self.mouseMods(forFlags: event.modifierFlags)
            let p = convert(event.locationInWindow, from: nil)
            let cw = max(cellSize.width, 1)
            let ch = max(cellSize.height, 1)
            // Clamp into the grid so a wheel event just off the bottom /
            // right edge still reports the last cell, not an out-of-range
            // coordinate.
            let x = Float(min(max(p.x, 0), cw * CGFloat(cols) - 1))
            let y = Float(min(max(p.y, 0), ch * CGFloat(rows) - 1))
            for _ in 0..<abs(rowDelta) {
                let bytes = mouseEncoder.encodeWheel(
                    button: button,
                    mods: mods,
                    x: x,
                    y: y,
                    screenWidth: UInt32(cw * CGFloat(cols)),
                    screenHeight: UInt32(ch * CGFloat(rows)),
                    cellWidth: UInt32(cw),
                    cellHeight: UInt32(ch)
                )
                if !bytes.isEmpty { onKey?(bytes) }
            }
            return
        }

        // Alt-screen: translate to arrow-key presses through the
        // key encoder. One arrow per row; respects DECCKM application
        // mode + Kitty keyboard flags because the encoder calls
        // `setopt_from_terminal` itself.
        if isAltScreenActive() {
            guard let keyEncoder else { return }
            let key: GhosttyKey = rowDelta > 0 ? GHOSTTY_KEY_ARROW_UP : GHOSTTY_KEY_ARROW_DOWN
            for _ in 0..<abs(rowDelta) {
                let bytes = keyEncoder.encode(syntheticKey: key)
                if !bytes.isEmpty { onKey?(bytes) }
            }
            return
        }

        // Primary screen + no mouse tracking → local viewport scroll.
        // The C delta sign is "up is negative" per terminal.h:201;
        // NSEvent.scrollingDeltaY is positive when the user pushes
        // the wheel up (which should scroll BACK in the buffer, i.e.
        // toward earlier rows). So we pass `-rowDelta` to scroll
        // back when delta is positive.
        var sv = GhosttyTerminalScrollViewport()
        sv.tag = GHOSTTY_SCROLL_VIEWPORT_DELTA
        sv.value.delta = intptr_t(-rowDelta)
        ghostty_terminal_scroll_viewport(terminal, sv)
        scrolledBack = true
        needsDisplay = true
    }

    /// Quantize an NSEvent scroll into whole-row delta. Smooth-scroll
    /// events accumulate fractional points; discrete wheel notches
    /// dispatch `rowsPerWheelNotch` rows each. Returns 0 when we
    /// haven't crossed a whole-row boundary yet (caller short-circuits).
    private func quantizeScrollDelta(event: NSEvent) -> Int {
        if event.hasPreciseScrollingDeltas {
            // Trackpad / Magic Mouse: scrollingDeltaY is in screen
            // points. Convert to row units via cell height, accumulate.
            let cellH = max(cellSize.height, 1)
            let delta = Double(event.scrollingDeltaY) / Double(cellH)
            // Reset accumulator on direction change so a quick flick
            // back doesn't carry stale fractional momentum.
            let direction: Int = delta > 0 ? 1 : (delta < 0 ? -1 : 0)
            if lastScrollDirection != 0,
               direction != 0,
               direction != lastScrollDirection
            {
                scrollAccum = 0
            }
            scrollAccum += delta
            let rows = Int(scrollAccum.rounded(.towardZero))
            if rows != 0 {
                scrollAccum -= Double(rows)
                lastScrollDirection = rows > 0 ? 1 : -1
            }
            return rows
        }
        // Discrete wheel: scrollingDeltaY is signed clicks. Bias by
        // rowsPerWheelNotch so a single click moves a noticeable
        // chunk (matches Go's session.go:794).
        let clicks = Int(event.scrollingDeltaY.rounded())
        scrollAccum = 0
        lastScrollDirection = clicks > 0 ? 1 : (clicks < 0 ? -1 : 0)
        return clicks * Self.rowsPerWheelNotch
    }

    /// Check whether the terminal currently has any mouse-tracking
    /// mode enabled (X10 / normal / button / any-event). When true,
    /// scroll-wheel events should be encoded as button-4/5 instead
    /// of local viewport scroll — deferred to a separate milestone.
    private func isMouseTrackingActive() -> Bool {
        guard let terminal else { return false }
        var active: Bool = false
        _ = ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, &active)
        return active
    }

    /// Translate NSEvent.modifierFlags to libghostty-vt's mods bitmask
    /// for a mouse report. Same bit layout as KeyEncoder's; duplicated
    /// here to keep the mouse path independent of the key encoder.
    private static func mouseMods(forFlags flags: NSEvent.ModifierFlags) -> GhosttyMods {
        var mods: UInt16 = 0
        if flags.contains(.shift)   { mods |= 1 << 0 } // GHOSTTY_MODS_SHIFT
        if flags.contains(.control) { mods |= 1 << 1 } // GHOSTTY_MODS_CTRL
        if flags.contains(.option)  { mods |= 1 << 2 } // GHOSTTY_MODS_ALT
        if flags.contains(.command) { mods |= 1 << 3 } // GHOSTTY_MODS_SUPER
        return GhosttyMods(mods)
    }

    /// Check whether the alt-screen is active (vim, less, etc.). The
    /// alt-screen has no scrollback by design, so wheel events
    /// translate to arrow-key presses for the focused app's own
    /// scroll handling.
    private func isAltScreenActive() -> Bool {
        guard let terminal else { return false }
        var screen: GhosttyTerminalScreen = GHOSTTY_TERMINAL_SCREEN_PRIMARY
        _ = ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &screen)
        return screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE
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
        // M2: read cursor state up-front so the walk can cache the
        // glyph at the cursor cell. Cached glyph is consumed by the
        // post-walk cursor-draw pass to re-paint the character in an
        // inverted color over a focused block cursor.
        let cursorInfo = renderState.cursor()
        cursorCellGlyph = nil
        cursorCellOriginalForeground = nil
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
            // M2: stash glyph at the cursor's cell so the cursor
            // pass can redraw it in an inverted color over a
            // focused block cursor. Done inline here to avoid a
            // second cell walk just for the cursor cell.
            if let cur = cursorInfo,
               cell.row == Int(cur.row),
               cell.col == Int(cur.col)
            {
                self.cursorCellGlyph = cell.glyph
                self.cursorCellOriginalForeground = cell.foreground ?? defaultFg
            }
        }

        // Cursor (goal-mac-polish-cursor-keys M2 + Claude-cursor follow-up).
        // Drawn AFTER glyphs but BEFORE selection — selection wants to
        // visually dominate the cursor cell when the user's mid-drag.
        //
        // **Visibility policy**: we deliberately diverge from strict
        // DECTCEM (mode 25) compliance when the view is focused. TUI
        // apps like Claude Code disable the system cursor and render
        // their own placeholder character — but the placeholder
        // disappears the moment the user starts typing, leaving no
        // indication of where input lands. We keep the system cursor
        // visible whenever the view is focused, regardless of
        // libghostty's `visible` flag, so the user can always see
        // where their next keystroke will land. This matches cmux's
        // behavior and is the UX the user requested.
        //
        // When the view is NOT focused we still defer to the
        // visibility flag — background tabs whose TUI apps have
        // hidden the cursor stay quiet (less visual noise).
        if let cur = cursorInfo, cur.visible || hasFocus {
            let cursorRect = NSRect(
                x: CGFloat(cur.col) * cellW,
                y: CGFloat(cur.row) * cellH,
                width: cellW,
                height: cellH
            )
            let cursorColor = cur.color ?? theme.cursor
            if !hasFocus {
                // Unfocused: hollow outline, always on (no blink).
                // Mirrors Go `cmd/roost/render.go:154-161`.
                drawCursorOutline(in: cursorRect, color: cursorColor)
            } else if cursorBlinkOn {
                // Focused + blink-on: visual style decides shape.
                // libghostty-vt can ask for BLOCK_HOLLOW directly
                // (e.g. some apps via DECSCUSR variants); honor it
                // by routing to the same outline path as blurred.
                switch cur.visualStyle {
                case GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR:
                    drawCursorBar(in: cursorRect, color: cursorColor)
                case GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE:
                    drawCursorUnderline(in: cursorRect, color: cursorColor)
                case GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW:
                    drawCursorOutline(in: cursorRect, color: cursorColor)
                case GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK:
                    fallthrough
                default:
                    drawCursorBlock(
                        in: cursorRect,
                        color: cursorColor,
                        cellFont: cellFont
                    )
                }
            }
            // Focused + !cursorBlinkOn → don't draw; cell shows through.
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

    // MARK: - Cursor draw helpers (M2)

    /// Solid block cursor with the underlying glyph re-painted in an
    /// inverted color so the character stays legible. Uses
    /// `theme.background` as the inverted-text color — fine for the
    /// fallback + bundled themes; if a theme ever ships a dedicated
    /// `cursorText` field we'd thread it through instead.
    private func drawCursorBlock(in rect: NSRect, color: NSColor, cellFont: NSFont) {
        color.setFill()
        rect.fill()
        guard let glyph = cursorCellGlyph, !glyph.isWhitespace else { return }
        let attrs: [NSAttributedString.Key: Any] = [
            .font: cellFont,
            .foregroundColor: theme.background,
        ]
        let line = NSAttributedString(string: String(glyph), attributes: attrs)
        line.draw(at: NSPoint(x: rect.minX, y: rect.minY))
    }

    /// DECSCUSR 5/6 — thin vertical bar at the left edge of the cell.
    /// 2pt wide is the standard convention (Terminal.app, iTerm).
    private func drawCursorBar(in rect: NSRect, color: NSColor) {
        color.setFill()
        NSRect(x: rect.minX, y: rect.minY, width: 2, height: rect.height).fill()
    }

    /// DECSCUSR 3/4 — horizontal underline at the cell's baseline.
    /// 2pt tall, sitting at the bottom edge.
    private func drawCursorUnderline(in rect: NSRect, color: NSColor) {
        color.setFill()
        NSRect(x: rect.minX, y: rect.maxY - 2, width: rect.width, height: 2).fill()
    }

    /// Hollow outline — used when the view doesn't have focus or
    /// when libghostty asks for `BLOCK_HOLLOW` explicitly. Insets
    /// slightly so a 1pt stroke fits inside the cell rect rather
    /// than bleeding over the neighbor's edge.
    private func drawCursorOutline(in rect: NSRect, color: NSColor) {
        color.setStroke()
        let outline = NSBezierPath(rect: rect.insetBy(dx: 0.5, dy: 0.5))
        outline.lineWidth = 1
        outline.stroke()
    }
}
