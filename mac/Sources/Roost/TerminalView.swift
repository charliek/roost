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
import QuartzCore  // CACurrentMediaTime — monotonic clock for the motion throttle.

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

    /// Cell-coordinate selection state. Two endpoints form the
    /// rectangle, normalized at draw + extract time so the user can
    /// drag in any direction.
    ///
    /// Rows are stored in libghostty's `PointTag::Screen` coordinate
    /// space — the unified screen-including-scrollback index that
    /// stays stable as the user scrolls. The viewport row equivalent
    /// is computed each frame in [`draw`] via
    /// `ghostty_terminal_grid_ref` so the highlight scrolls with the
    /// content (matching Ghostty / cmux behavior). Cols are
    /// column-index integers that don't change with vertical scroll.
    private struct CellSelection {
        var anchorCol: Int
        var anchorScreenY: UInt32
        var cursorCol: Int
        var cursorScreenY: UInt32
        /// True when the selection was set as a deliberate commit
        /// (multi-click n_press dispatch, `setSelection` from IPC)
        /// rather than as a single-cell `mouseDown` anchor that the
        /// user hasn't dragged yet. Codex flagged on PR #177 that a
        /// double-click on a single-letter word (e.g. `i`) returns a
        /// single-cell span — geometrically equal to a
        /// click-without-drag, but the user expects to see + copy
        /// it. `committed` is the bit that distinguishes the two.
        var committed: Bool = false

        /// True if the anchor and cursor land on the same cell —
        /// shouldn't render selection chrome for a single-cell
        /// "click but didn't drag" gesture (unless it was committed
        /// via multi-click; see `isVisible`).
        var isEmpty: Bool { anchorCol == cursorCol && anchorScreenY == cursorScreenY }

        /// Should the renderer paint this selection / copy-on-select
        /// write the pasteboard? A committed single-cell span paints;
        /// an in-progress single-cell drag does not.
        var isVisible: Bool { committed || !isEmpty }

        /// Inclusive (startY, startCol) and exclusive (endY, endCol)
        /// in screen-row space.
        func normalized() -> (startY: UInt32, startCol: Int, endY: UInt32, endCol: Int) {
            if anchorScreenY == cursorScreenY {
                return (
                    anchorScreenY,
                    min(anchorCol, cursorCol),
                    anchorScreenY &+ 1,
                    max(anchorCol, cursorCol) + 1
                )
            } else if anchorScreenY < cursorScreenY {
                return (anchorScreenY, anchorCol, cursorScreenY &+ 1, cursorCol + 1)
            } else {
                return (cursorScreenY, cursorCol, anchorScreenY &+ 1, anchorCol + 1)
            }
        }
    }

    private var selection: CellSelection?

    // MARK: - Clickable links (PR B)

    /// Live hover-URL state. `nil` when the pointer is not over a URL
    /// or when the Cmd modifier isn't held. Populated by `mouseMoved`
    /// + `flagsChanged` from either an OSC 8 hyperlink lookup or a
    /// regex match on the row text under the pointer.
    ///
    /// Stored row is the **viewport** row (0-indexed). The underline
    /// + click handler both consume it directly; if the user scrolls
    /// while hovering, the next mouseMoved invalidates and recomputes.
    private struct HoverURL: Equatable {
        let col0: Int
        let col1: Int
        let row: Int
        let url: String
    }
    private var hoverURL: HoverURL?

    /// Last known Cmd-modifier state. Updated in `flagsChanged`; read
    /// by hover + click + draw so the underline + hand cursor track
    /// the modifier press/release in real time.
    private var commandHeld: Bool = false

    /// Plug-in launcher for the Cmd-click handler. Default routes to
    /// `NSWorkspace.shared.open`; tests substitute a custom launcher
    /// to assert without launching a real browser.
    @MainActor var urlLauncher: UrlLauncher = WorkspaceUrlLauncher()

    /// Set during `mouseDown` when the Cmd-click short-circuit fires
    /// (URL opened via `urlLauncher.open`). Read by `mouseDragged` +
    /// `mouseUp` so the gesture skips the selection-drag path and the
    /// trailing copy-on-select. Cleared by `mouseUp` once consumed,
    /// preserving any prior selection that existed before the Cmd-
    /// click began.
    private var linkClickConsumedThisGesture: Bool = false

    /// Set during `mouseDown` when `handleClickCount` commits a
    /// word/line selection. Read by `mouseUp` so the gesture skips
    /// (a) the "anchor == cursor → clear" branch (a single-cell
    /// span — e.g. double-clicking the letter `i` or triple-clicking
    /// a one-character row — would otherwise be wiped immediately on
    /// release; Codex flagged this on PR #176) and (b) the copy-on-
    /// select branch (handleClickCount already wrote the selection
    /// to the pasteboard — avoids the redundant write Codex also
    /// flagged). Cleared by `mouseUp` once consumed.
    private var multiClickConsumedThisGesture: Bool = false

    /// Tracking area covering the full view bounds. Required so
    /// `mouseMoved` fires even when the user isn't dragging — hover
    /// detection wouldn't work otherwise. Rebuilt in
    /// `updateTrackingAreas` on bounds change.
    nonisolated(unsafe) private var hoverTrackingArea: NSTrackingArea?

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

    // MARK: - Scrollback (goal-mac-polish-cursor-keys M6)

    /// Fractional accumulator for trackpad smooth-scroll. NSEvent's
    /// `scrollingDeltaY` is in points (continuous on Magic Mouse /
    /// trackpads). We translate to rows by dividing by `cellSize.height`,
    /// accumulate the fractional remainder, dispatch whole rows when
    /// `|accum|` crosses a row boundary. Reset on direction change so
    /// the user can quickly flick back without overshoot.
    private var scrollAccum: Double = 0.0
    private var lastScrollDirection: Int = 0

    /// Throttle for mode 1003 motion-no-button reports. Caps the
    /// PTY's motion-byte rate to 60 Hz and skips per-cell duplicates.
    /// Lives on TerminalView so the per-tab throttle state doesn't
    /// leak across tabs. See `MotionEmitter` in MouseRouting.swift.
    private var motionEmitter = MotionEmitter()

    /// Last applied W3C cursor name from OSC 22. Empty string (or
    /// `"default"`) means the platform default arrow. Strix sends
    /// `pointer` while hovering its split bar and `default` to reset.
    /// Read by `cursorUpdate(with:)` and the `app.cursor_shape` IPC
    /// op (Tier 1 OSC 22 wiring). Reset to empty on alt-screen exit
    /// so a hung TUI's stale `pointer` doesn't survive across tab
    /// teardown / shell reset.
    private var currentOscShape: String = ""

    /// Tracks the prior alt-screen state so `appendBytes` can detect
    /// an alt→primary transition and reset `currentOscShape`. xterm
    /// convention: a TUI that owns the alt screen owns the cursor
    /// shape with it; once the alt screen exits, the platform
    /// default returns.
    private var wasAltScreenActive: Bool = false

    /// `true` when the viewport has been scrolled away from the bottom
    /// by a local-scroll event. Cleared by the snap-to-bottom hook in
    /// `keyDown`, which fires `GHOSTTY_SCROLL_VIEWPORT_BOTTOM` before
    /// the keystroke reaches libghostty. Mirrors Go
    /// `cmd/roost/input.go:67` ("Snap viewport before delivering the
    /// keystroke") + `cmd/roost/session.go:108-112` (`scrolledBack`).
    private var scrolledBack: Bool = false

    /// `copy-on-select` mode. Read from `RoostConfig.copyOnSelect`
    /// on tab creation and passed through here so mouseUp /
    /// otherMouseDown can branch on it. Default matches the config
    /// default (`.on`).
    var copyOnSelect: CopyOnSelect = .default

    /// `clipboard-write` policy. Read from `RoostConfig.clipboardWrite`
    /// at tab creation. Checked in `appendBytes`'s OSC 52 short-circuit
    /// to decide whether a program-initiated clipboard write goes
    /// through. Default `.allow` (matches Ghostty).
    var clipboardWritePolicy: ClipboardWrite = .default

    /// Extra word-char set used by double-click word expansion in
    /// `mouseDown(with:)`. Read from `RoostConfig.wordBreakChars` at
    /// tab creation. Default matches Ghostty's `_-.+~/:@%` —
    /// keeps file paths + URLs whole on double-click.
    var wordBreakChars: String = WordSelection.defaultWordChars

    /// Custom-named selection pasteboard for `copy-on-select = .on`.
    /// Mirrors cmux's `com.mitchellh.ghostty.selection` pattern:
    /// drag-to-select writes here and middle-click in any Roost
    /// terminal reads from here, all without touching the system
    /// pasteboard that ⌘V reads from.
    static let selectionPasteboard = NSPasteboard(
        name: NSPasteboard.Name("ai.stridelabs.Roost.selection")
    )

    init(
        cols: UInt16 = 80,
        rows: UInt16 = 24,
        theme: Theme = .fallback,
        font: NSFont = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular),
        copyOnSelect: CopyOnSelect = .default,
        clipboardWrite: ClipboardWrite = .default,
        wordBreakChars: String = WordSelection.defaultWordChars
    ) {
        self.cols = cols
        self.rows = rows
        self.theme = theme
        self.copyOnSelect = copyOnSelect
        self.clipboardWritePolicy = clipboardWrite
        self.wordBreakChars = wordBreakChars

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
            // Synthesise OSC 10/11/12 query replies inline —
            // libghostty-vt drops the .query arm of its color-op
            // handler, so without us answering, codex (and reportedly
            // claude-code) skip their prompt-row bg SGR sequence. We
            // route the reply through `onKey` because the destination
            // is exactly the same: bytes injected into the PTY's
            // stdin alongside user keystrokes via the tab's
            // keystroke continuation — FIFO with other writes *once
            // enqueued*, not against PTY output that hasn't been
            // drained yet. Reads libghostty's *currently effective*
            // color so a prior `OSC 10/11/12;rgb:…` set is reflected
            // in the next query reply (vim colorscheme plugins etc.).
            // Mirrors the Linux drain at
            // `crates/roost-linux/src/app.rs` and the legacy Go
            // reference at `internal/osc/scanner.go:280-300`.
            if case .colorQuery(let n) = event {
                let color = TerminalView.liveColor(forQuery: n, terminal: terminal, theme: theme)
                if let color = color,
                   let reply = TerminalView.formatColorQueryResponse(n: n, color: color)
                {
                    onKey?(reply)
                }
                continue
            }
            // OSC 52 program-initiated clipboard write — UI-only
            // action, not workspace state. Honor on the UI side
            // because only the UI has the NSPasteboard handle.
            // `clipboard-write = .deny` drops silently + logs,
            // matching Ghostty (Surface.zig:2164-2166).
            if case .clipboard(let target, let text) = event {
                if clipboardWritePolicy == .deny {
                    NSLog(
                        "roost-mac: OSC 52 clipboard write dropped — clipboard-write = deny"
                    )
                    continue
                }
                let pb: NSPasteboard = (target == .system)
                    ? NSPasteboard.general
                    : Self.selectionPasteboard
                pb.clearContents()
                pb.setString(text, forType: .string)
                continue
            }
            // OSC 22 cursor shape — UI-only action. Strix uses this
            // for the divider grab; kitty et al. for broader hover
            // affordances. Empty body and unknown names both fall
            // back to the platform default arrow on
            // `cursorUpdate(with:)`. Store the raw name (canonicalised
            // for the IPC reader) so the next tracking-area refresh
            // applies it.
            if case .mouseShape(let name) = event {
                currentOscShape = name
                applyCurrentCursorShapeIfNeeded()
                continue
            }
            guard let (cmd, payload) = event.asReport else {
                continue
            }
            onOsc?(cmd, payload)
        }
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            ghostty_terminal_vt_write(terminal, base, data.count)
        }
        // Alt→primary transition resets the OSC 22 cursor shape so a
        // TUI that owned the alt screen with a `pointer` doesn't
        // leave the cursor stuck on shell prompt. Matches xterm /
        // ghostty conventions.
        let altNow = isAltScreenActive()
        if wasAltScreenActive, !altNow, !currentOscShape.isEmpty {
            currentOscShape = ""
            applyCurrentCursorShapeIfNeeded()
        }
        wasAltScreenActive = altNow
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
            // Reparenting away from a window (tab close, tear-down)
            // strands hover state. Clear it so the next mount starts
            // fresh — same rationale as the cursor blink stop.
            clearLinkHoverState()
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
        // Mode 1004 focus tracking: report focus-in to the app. vim,
        // less, btop and similar TUIs use this to redraw on focus
        // state changes. Emit only when the app actually enabled
        // the mode — otherwise the bytes would land at the shell
        // prompt as visible junk.
        emitFocusEvent(focused: true)
        needsDisplay = true
    }

    @objc private func handleWindowDidResignKey(_ note: Notification) {
        windowIsKey = false
        updateBlinkTimerForFocus()
        // Cmd-Tab away from the window strands the hover state — the
        // user can't see the cursor change and the underline is
        // misleading on a background window. Clear so the next
        // focus-in starts fresh.
        clearLinkHoverState()
        emitFocusEvent(focused: false)
        needsDisplay = true
    }

    /// Write the xterm focus-tracking sequence onto the PTY input
    /// channel when mode 1004 is enabled. Uses libghostty-vt's
    /// `ghostty_focus_encode` so the wire format follows the same
    /// authoritative source as ghostty itself (CSI I for gained,
    /// CSI O for lost).
    ///
    /// Background tabs' TerminalViews each observe the same window-
    /// key notifications, so without gating on first-responder
    /// status every Cmd-Tab would write focus events to every
    /// open mode-1004 TUI. `requireFirstResponder=true` (the default
    /// for the window-notification path) keeps the emit on the
    /// active tab only. The IPC test seam passes `false` because
    /// `app.set_window_focus` already targets the active session
    /// upstream.
    @MainActor
    func emitFocusEvent(focused: Bool, requireFirstResponder: Bool = true) {
        guard isFocusTrackingActive() else { return }
        if requireFirstResponder, !viewIsFirstResponder { return }
        let event: GhosttyFocusEvent = focused ? GHOSTTY_FOCUS_GAINED : GHOSTTY_FOCUS_LOST
        var buf = [CChar](repeating: 0, count: 8)
        var written: size_t = 0
        let rc = buf.withUnsafeMutableBufferPointer { p in
            ghostty_focus_encode(event, p.baseAddress, p.count, &written)
        }
        guard rc.rawValue == GHOSTTY_SUCCESS.rawValue, written > 0 else { return }
        let data = buf.prefix(written).withUnsafeBufferPointer { ptr in
            Data(bytes: ptr.baseAddress!, count: written)
        }
        onKey?(data)
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
            // Same rationale as `handleWindowDidResignKey`: focus
            // shifted away from this view, so the hover state is
            // misleading.
            clearLinkHoverState()
            needsDisplay = true
        }
        return resigned
    }

    /// Drop any active link-hover state and restore the default I-beam
    /// cursor. Called on focus loss (window resign-key, view resign-
    /// first-responder, window detach) so the underline + hand cursor
    /// don't survive past the moment the user can act on them.
    @MainActor
    private func clearLinkHoverState() {
        commandHeld = false
        if hoverURL != nil {
            hoverURL = nil
        }
        updateLinkCursor()
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

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let existing = hoverTrackingArea {
            removeTrackingArea(existing)
        }
        // Cover the full visible bounds. `.activeAlways` so hover
        // fires even when the window isn't key — Cmd-hover is a peek
        // gesture and users expect the underline + hand cursor without
        // first clicking the window. `.mouseMoved` is what makes
        // `mouseMoved(with:)` deliver events at all (default tracking
        // only fires enter/exit). `.inVisibleRect` lets AppKit clip
        // the rect to the visible portion automatically.
        let area = NSTrackingArea(
            rect: bounds,
            options: [.mouseMoved, .activeAlways, .inVisibleRect, .mouseEnteredAndExited],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(area)
        hoverTrackingArea = area
    }

    override func mouseEntered(with event: NSEvent) {
        super.mouseEntered(with: event)
        pointerIsOverThisView = true
        // A background-tab OSC 22 drain may have set
        // `currentOscShape` while we were unfocused; apply now that
        // the pointer is over us so the shape lands without waiting
        // for the next mouseMoved.
        applyCurrentCursorShapeIfNeeded()
    }

    override func mouseExited(with event: NSEvent) {
        super.mouseExited(with: event)
        pointerIsOverThisView = false
        if hoverURL != nil {
            hoverURL = nil
            // No `.set()` call: the pointer's now in another tracking
            // area (or outside the window), and changing the OS
            // cursor here would race the destination's
            // `cursorUpdate`. Redraw to drop the URL underline.
            needsDisplay = true
        }
    }

    override func mouseMoved(with event: NSEvent) {
        super.mouseMoved(with: event)
        commandHeld = event.modifierFlags.contains(.command)
        let point = convert(event.locationInWindow, from: nil)
        // URL hover detection runs regardless of mode — Cmd-hover is
        // a peek gesture and the underline + hand cursor are useful
        // even when a TUI has mouse tracking enabled.
        recomputeHoverURL(at: point)
        // Mode 1003 (any-event) motion: report movement to the PTY
        // with no button held. The 60 Hz cap + per-cell dedup lives
        // inside `emitMouseTracking` so the IPC test seam exercises
        // it too.
        guard isMouseTrackingActive(), isAnyEventMotionTrackingActive() else { return }
        emitMouseTracking(
            action: .motion,
            button: nil,
            mods: Self.mouseMods(forFlags: event.modifierFlags),
            point: point
        )
    }

    override func mouseDragged(with event: NSEvent) {
        // A consumed Cmd-click already opened the URL; the up event
        // is the only thing left, and selection state must stay
        // untouched.
        if linkClickConsumedThisGesture { return }
        commandHeld = event.modifierFlags.contains(.command)
        let p = convert(event.locationInWindow, from: nil)
        // Drag past the view edge — `cellAt` clamps to the nearest
        // valid cell, which would otherwise keep the underline +
        // hand cursor alive over an edge URL. Out-of-bounds drag
        // → clear hover before the recompute can resurrect it.
        if !bounds.contains(p) {
            if hoverURL != nil {
                hoverURL = nil
                updateLinkCursor()
                needsDisplay = true
            }
        } else {
            recomputeHoverURL(at: p)
        }
        // Mouse-tracking apps under mode 1002 (button-event) want
        // drag motion reports. The encoder declines (returns empty)
        // when the mode is 1000-only, so this is a safe call to make
        // any time tracking is on.
        if forwardMouseEventToTracking(
            kind: .motion,
            button: .left,
            event: event
        ) {
            return
        }
        selectionMouseDragged(event)
    }

    override func flagsChanged(with event: NSEvent) {
        super.flagsChanged(with: event)
        let nowHeld = event.modifierFlags.contains(.command)
        guard nowHeld != commandHeld else { return }
        commandHeld = nowHeld
        // Recompute hover state at the pointer's current position
        // because the underline + cursor depend on `commandHeld`.
        if let win = window {
            let inWindow = win.mouseLocationOutsideOfEventStream
            let p = convert(inWindow, from: nil)
            if bounds.contains(p) {
                recomputeHoverURL(at: p)
            } else if hoverURL != nil {
                hoverURL = nil
                updateLinkCursor()
                needsDisplay = true
            }
        }
    }

    /// Compute the URL covering the cell at `point`, if Cmd is held.
    /// Prefers OSC 8 explicit hyperlinks over a regex match on the
    /// row text — a shell that emits `\e]8;;URI\e\\…` decides what
    /// "the URL" is regardless of what the cell text looks like.
    /// Updates `hoverURL`, the cursor, and triggers a redraw.
    @MainActor
    private func recomputeHoverURL(at point: NSPoint) {
        guard commandHeld, let terminal else {
            if hoverURL != nil {
                hoverURL = nil
                updateLinkCursor()
                needsDisplay = true
            }
            return
        }
        let (col, row) = cellAt(point: point)
        let next = computeHoverURL(terminal: terminal, col: col, row: row)
        if next != hoverURL {
            hoverURL = next
            updateLinkCursor()
            needsDisplay = true
        }
    }

    /// Resolve the URL (if any) covering `(col, row)`. OSC 8 wins
    /// over regex: if the cell carries an explicit hyperlink, the
    /// span is the contiguous run of cells sharing that URI; the
    /// regex pass is skipped. Otherwise we build the row's text by
    /// walking the render state and let `UrlDetection.find` answer.
    @MainActor
    private func computeHoverURL(
        terminal: GhosttyTerminal,
        col: Int,
        row: Int
    ) -> HoverURL? {
        // Prefer OSC 8 explicit hyperlinks.
        if let uri = UrlDetection.hyperlinkAt(terminal: terminal, col: col, row: row) {
            let (c0, c1) = osc8SpanAt(terminal: terminal, col: col, row: row, uri: uri)
            return HoverURL(col0: c0, col1: c1, row: row, url: uri)
        }
        // Regex fallback: assemble the row text + look up the column.
        let rowText = textForViewportRow(row)
        if let span = UrlDetection.find(in: rowText, at: col) {
            return HoverURL(col0: span.col0, col1: span.col1, row: row, url: span.url)
        }
        return nil
    }

    /// Walk the OSC 8 hyperlink span outward from `(col, row)` so the
    /// underline + click-target cover every cell that shares the same
    /// URI. libghostty only answers per-cell; the contiguous-span walk
    /// is the renderer's job. Stops when the URI changes or runs out
    /// — we don't cross row boundaries here (linewrap is a TODO).
    @MainActor
    private func osc8SpanAt(
        terminal: GhosttyTerminal,
        col: Int,
        row: Int,
        uri: String
    ) -> (col0: Int, col1: Int) {
        var c0 = col
        while c0 > 0,
              UrlDetection.hyperlinkAt(terminal: terminal, col: c0 - 1, row: row) == uri
        {
            c0 -= 1
        }
        var c1 = col
        let maxCol = Int(cols) - 1
        while c1 < maxCol,
              UrlDetection.hyperlinkAt(terminal: terminal, col: c1 + 1, row: row) == uri
        {
            c1 += 1
        }
        return (c0, c1)
    }

    /// Test-only re-export of `textForViewportRow` so the
    /// `tab.expand_selection_at` IPC bridge can re-derive the (col0,
    /// col1) span from the same row text the production dispatch
    /// walked. Pure read; no state mutation beyond what the
    /// production code path already does.
    @MainActor
    func viewportRowTextForTest(row: Int) -> String {
        textForViewportRow(row)
    }

    /// Build the visible text of one viewport row by walking the
    /// render state. Each cell contributes exactly **one Unicode
    /// scalar** so the click column (which the renderer reports in
    /// cell units) lines up with `unicodeScalars[col]` in
    /// `WordSelection`/`UrlDetection` — codex flagged this on PR #176
    /// after noticing that `e\u{0301} foo` would otherwise put scalar
    /// index 2 on the space, not on `f`. We emit the grapheme's
    /// first scalar and drop any trailing combining marks; the
    /// terminal cell is one display column regardless, so the lossy
    /// reduction only affects what the algorithms see (no glyph is
    /// drawn from this string). Empty cells fall through as a single
    /// space.
    ///
    /// Same shape as `selectedPlainText` / `dumpText`, narrowed to
    /// a single row.
    @MainActor
    private func textForViewportRow(_ row: Int) -> String {
        guard let terminal else { return "" }
        renderState.update(terminal: terminal)
        var scalars = String.UnicodeScalarView()
        renderState.walk { cell in
            guard cell.row == row else { return }
            if let g = cell.glyph, let first = g.unicodeScalars.first {
                scalars.append(first)
            } else {
                scalars.append(" ")
            }
        }
        return String(scalars)
    }

    /// Apply the right cursor for the current state. AppKit's cursor
    /// stack can drift; calling `.set()` directly is the most
    /// reliable path — `cursorUpdate(with:)` only fires on
    /// tracking-area boundary crossings, not on internal state
    /// changes like a modifier press.
    ///
    /// Precedence (highest → lowest):
    ///   1. Cmd-hover over a URL → `.pointingHand` (URL beats OSC 22).
    ///   2. OSC 22 cursor shape set by the running TUI → mapped via
    ///      `nsCursorForW3CName`.
    ///   3. Default → `.iBeam` (the terminal's "I-can-select-text" cue).
    @MainActor
    private func updateLinkCursor() {
        applyCurrentCursorShapeIfNeeded()
    }

    /// Apply the cursor implied by `commandHeld + hoverURL +
    /// currentOscShape`. Called from the URL-hover recompute and
    /// from the OSC 22 drain — both can change the right answer.
    ///
    /// Only mutates the system cursor when the pointer is inside
    /// THIS view's tracking area. Without that gate a background
    /// tab's OSC 22 drain would re-apply a process-global cursor
    /// while the pointer is over another tab (or another app).
    /// The view's own `cursorUpdate(with:)` override re-applies
    /// on tracking-area boundary crossings, so on the next enter
    /// the right cursor lands.
    @MainActor
    private func applyCurrentCursorShapeIfNeeded() {
        guard pointerIsOverThisView else { return }
        if hoverURL != nil && commandHeld {
            NSCursor.pointingHand.set()
            return
        }
        if !currentOscShape.isEmpty {
            nsCursorForW3CName(currentOscShape).set()
            return
        }
        NSCursor.iBeam.set()
    }

    /// Tracks whether the pointer is currently inside our hover
    /// tracking area. Updated by `mouseEntered` / `mouseExited`
    /// (the tracking area's `.mouseEnteredAndExited` flag), so a
    /// background-tab OSC 22 drain doesn't reach NSCursor while the
    /// pointer is elsewhere.
    private var pointerIsOverThisView: Bool = false

    /// AppKit calls this when the pointer crosses our tracking area.
    /// Without this override the OS would reset to `.arrow` on every
    /// crossing; here we re-apply the active cursor so OSC 22 + URL
    /// hover survive the boundary.
    override func cursorUpdate(with event: NSEvent) {
        applyCurrentCursorShapeIfNeeded()
    }

    /// Read-only accessor for the active OSC 22 shape (canonicalised
    /// — empty body and `"default"` both return `"default"`). Used by
    /// the `app.cursor_shape` IPC op so test clients can assert the
    /// renderer actually consumed an OSC 22 payload.
    @MainActor
    func currentCursorShapeName() -> String {
        return canonicalCursorShape(currentOscShape)
    }

    /// Cmd-click on a URL opens it; everything else falls through to
    /// the regular selection-drag handler. Pull the selection logic
    /// out of `mouseDown` into a helper so the Cmd-click short-circuit
    /// can return cleanly without manually re-asserting selection
    /// state.
    private func selectionMouseDown(_ event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        let cell = cellAt(point: p)
        guard let screenY = screenY(forViewportRow: cell.row) else {
            // libghostty rejected the viewport→screen conversion (very
            // narrow window: tab tearing down, no terminal handle yet).
            // Clear any stale selection so the user isn't left with a
            // highlight pointing at the wrong row.
            if selection != nil {
                selection = nil
                needsDisplay = true
            }
            return
        }
        selection = CellSelection(
            anchorCol: cell.col,
            anchorScreenY: screenY,
            cursorCol: cell.col,
            cursorScreenY: screenY
        )
        needsDisplay = true
    }

    private func selectionMouseDragged(_ event: NSEvent) {
        guard var sel = selection else { return }
        let p = convert(event.locationInWindow, from: nil)
        let cell = cellAt(point: p)
        guard let screenY = screenY(forViewportRow: cell.row) else {
            // Mid-drag conversion failure (rare — usually means the
            // terminal handle just went away). Drop the selection
            // rather than continue updating a stale anchor.
            selection = nil
            needsDisplay = true
            return
        }
        sel.cursorCol = cell.col
        sel.cursorScreenY = screenY
        selection = sel
        needsDisplay = true
    }

    override func mouseDown(with event: NSEvent) {
        commandHeld = event.modifierFlags.contains(.command)
        linkClickConsumedThisGesture = false
        multiClickConsumedThisGesture = false
        let p = convert(event.locationInWindow, from: nil)
        let (col, row) = cellAt(point: p)
        // Cmd-click on a URL wins over word/line expansion even at
        // clickCount 2 / 3 — Cmd is the explicit "open this link"
        // gesture; the user's still right-handed about which one
        // they meant. Matches the cmux behavior. URL precedence also
        // wins over mouse-tracking forwarding (matches ghostty).
        if handleLinkClick(col: col, row: row, commandHeld: commandHeld) {
            linkClickConsumedThisGesture = true
            return
        }
        // Mouse-tracking apps (TUIs with `\x1b[?1000h` / `?1002h`
        // enabled) get press reports at the pointer's cell. libvt's
        // encoder gates internally on the negotiated mode + format;
        // an empty return when the mode declines is its job.
        if forwardMouseEventToTracking(
            kind: .press,
            button: .left,
            event: event
        ) {
            return
        }
        let clickCount = max(1, event.clickCount)
        if clickCount >= 2 {
            if handleClickCount(col: col, row: row, clickCount: clickCount) {
                multiClickConsumedThisGesture = true
                return
            }
        }
        selectionMouseDown(event)
    }

    /// Pure dispatch for double- (word) and triple-click (line)
    /// selection. Returns `true` when the click was consumed and the
    /// selection was set; `false` when the caller should fall through
    /// to the single-cell selection path (whitespace double-click, or
    /// any `clickCount <= 1`). Extracted into a non-NSEvent function so
    /// swift-testing cases can drive the same path without fabricating
    /// an `NSEvent` — mirrors `handleLinkClick`'s test-seam shape.
    @MainActor
    @discardableResult
    func handleClickCount(col: Int, row: Int, clickCount: Int) -> Bool {
        guard clickCount >= 2 else { return false }
        let rowText = textForViewportRow(row)
        let span: WordSpan?
        if clickCount == 2 {
            span = WordSelection.expandWord(
                in: rowText,
                at: col,
                extraWordChars: wordBreakChars
            )
        } else {
            // 3+ → line expansion. ghostty and iTerm2 both treat
            // four-and-up-clicks as a no-op extending the triple-
            // click selection; we degrade to the same line span.
            span = WordSelection.expandLine(in: rowText)
        }
        guard let s = span else { return false }
        let ok = setSelection(
            anchorCol: s.col0,
            anchorRow: row,
            cursorCol: s.col1,
            cursorRow: row
        )
        guard ok else { return false }
        // Copy-on-select. Drag-end fires `mouseUp` which already runs
        // the copy path against `selection`; double/triple-click
        // doesn't drag, so we run the same copy step here. Same
        // pasteboard targets + same selectedPlainText source as the
        // mouseUp branch — kept in sync so a future config policy
        // tweak only has to land once on either path.
        if copyOnSelect != .off,
           let text = selectedPlainText(),
           !text.isEmpty
        {
            switch copyOnSelect {
            case .off: break
            case .on:
                Self.selectionPasteboard.clearContents()
                Self.selectionPasteboard.setString(text, forType: .string)
            case .clipboard:
                Self.selectionPasteboard.clearContents()
                Self.selectionPasteboard.setString(text, forType: .string)
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(text, forType: .string)
            }
        }
        return true
    }

    /// Pure click-handling for clickable links. Returns `true` when
    /// the click was consumed (URL opened); `false` when the caller
    /// should fall through to the regular selection-drag path.
    ///
    /// Extracted into a non-NSEvent function so swift-testing cases
    /// can drive the same path without constructing an `NSEvent`
    /// (which is awkward to fabricate in unit tests). The production
    /// `mouseDown` is a thin wrapper that pulls col/row from the
    /// event and delegates here.
    @MainActor
    @discardableResult
    func handleLinkClick(col: Int, row: Int, commandHeld: Bool) -> Bool {
        guard commandHeld, let terminal else { return false }
        self.commandHeld = true
        guard let hov = computeHoverURL(terminal: terminal, col: col, row: row),
              hov.row == row,
              col >= hov.col0,
              col <= hov.col1
        else { return false }
        // The pointer was over a URL the UI advertised via underline +
        // hand cursor. Even if `URL(string:)` rejects the string (rare
        // — malformed OSC 8 URI, exotic scheme), eat the click so we
        // don't surprise the user with a stray selection drag after
        // they thought they were following a link. Log + no-op.
        hoverURL = hov
        guard let url = URL(string: hov.url) else {
            NSLog("roost-mac: Cmd-click on unparseable URL %@", hov.url)
            return true
        }
        _ = urlLauncher.open(url)
        return true
    }

    /// Convert a viewport row (0-indexed from top of visible area) to
    /// its `PointTag::Screen` y coordinate. Returns nil if the row is
    /// out of range or libghostty rejects the conversion. Used by
    /// `mouseDown` / `mouseDragged` to anchor selection in
    /// scrollback-stable coordinates.
    @MainActor
    private func screenY(forViewportRow row: Int) -> UInt32? {
        guard let terminal else { return nil }
        guard row >= 0, row < Int(rows) else { return nil }
        var pt = GhosttyPoint()
        pt.tag = GHOSTTY_POINT_TAG_VIEWPORT
        pt.value.coordinate.x = 0
        pt.value.coordinate.y = UInt32(row)
        var gref = GhosttyGridRef()
        gref.size = MemoryLayout<GhosttyGridRef>.size
        guard ghostty_terminal_grid_ref(terminal, pt, &gref) == GHOSTTY_SUCCESS else {
            return nil
        }
        var out = GhosttyPointCoordinate()
        guard
            ghostty_terminal_point_from_grid_ref(
                terminal, &gref, GHOSTTY_POINT_TAG_SCREEN, &out
            ) == GHOSTTY_SUCCESS
        else { return nil }
        return out.y
    }

    /// Convert a `PointTag::Screen` y coordinate back to its current
    /// viewport row. Returns nil if the row is currently outside the
    /// visible viewport (scrolled into history above or below), in
    /// which case the caller should clip / skip.
    @MainActor
    private func viewportRow(forScreenY screenY: UInt32) -> Int? {
        guard let terminal else { return nil }
        var pt = GhosttyPoint()
        pt.tag = GHOSTTY_POINT_TAG_SCREEN
        pt.value.coordinate.x = 0
        pt.value.coordinate.y = screenY
        var gref = GhosttyGridRef()
        gref.size = MemoryLayout<GhosttyGridRef>.size
        guard ghostty_terminal_grid_ref(terminal, pt, &gref) == GHOSTTY_SUCCESS else {
            return nil
        }
        var out = GhosttyPointCoordinate()
        guard
            ghostty_terminal_point_from_grid_ref(
                terminal, &gref, GHOSTTY_POINT_TAG_VIEWPORT, &out
            ) == GHOSTTY_SUCCESS
        else { return nil }
        let v = Int(out.y)
        guard v >= 0, v < Int(rows) else { return nil }
        return v
    }

    override func mouseUp(with event: NSEvent) {
        // PR B: a Cmd-click that opened a URL must not run
        // copy-on-select against any prior selection that happened to
        // be live before the click. Eat the up-event, clear the
        // gesture flag, and preserve `selection` so the prior copy
        // stays intact.
        if linkClickConsumedThisGesture {
            linkClickConsumedThisGesture = false
            return
        }
        // PR #176: a double/triple-click that committed a word/line
        // selection through `handleClickCount` is already done — the
        // pasteboard write fired inside that helper, and a single-cell
        // span (e.g. double-clicking the letter `i`) must survive the
        // release instead of getting cleared by the "anchor == cursor
        // → empty" branch below.
        if multiClickConsumedThisGesture {
            multiClickConsumedThisGesture = false
            return
        }
        // Mirror the mouseDown forward: when mouse tracking is on,
        // release reports go to the encoder, NOT to copy-on-select.
        if forwardMouseEventToTracking(
            kind: .release,
            button: .left,
            event: event
        ) {
            return
        }
        // If the drag never moved (anchor == cursor) AND the selection
        // wasn't committed via multi-click / IPC, clear it — a
        // single-cell "click but didn't drag" shouldn't leave a stray
        // highlight. Committed single-cell selections (Codex PR #177)
        // and real multi-cell selections persist until the next
        // mouseDown or `clearSelection()`.
        if let sel = selection, !sel.isVisible {
            selection = nil
            needsDisplay = true
            return
        }
        // Copy-on-select. The three-state config follows Ghostty's
        // semantics; see docs/reference/config.md for the user-facing
        // explanation. The `.on` case writes only to the named
        // selection pasteboard — ⌘V in another app is intentionally
        // not affected; middle-click inside Roost reads from there.
        // `.clipboard` ALSO writes to the system pasteboard, so a
        // drag-and-paste-into-another-app flow works.
        if copyOnSelect != .off,
           let text = selectedPlainText(),
           !text.isEmpty
        {
            switch copyOnSelect {
            case .off:
                break
            case .on:
                Self.selectionPasteboard.clearContents()
                Self.selectionPasteboard.setString(text, forType: .string)
            case .clipboard:
                Self.selectionPasteboard.clearContents()
                Self.selectionPasteboard.setString(text, forType: .string)
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(text, forType: .string)
            }
        }
    }

    /// Right-button press → forward to the encoder when mouse
    /// tracking is on; otherwise no-op (no context menu yet — Tier 3
    /// scope). When the encoder declines (e.g. mode is 1000-only and
    /// the negotiated set doesn't include right-button), the bytes
    /// are empty and the event silently passes through.
    override func rightMouseDown(with event: NSEvent) {
        if forwardMouseEventToTracking(
            kind: .press,
            button: .right,
            event: event
        ) {
            return
        }
        super.rightMouseDown(with: event)
    }

    /// Right-button release: pair with `rightMouseDown` so apps
    /// using mode 1006 (SGR) get the explicit release event with the
    /// right-button identifier. Outside tracking, fall through.
    override func rightMouseUp(with event: NSEvent) {
        if forwardMouseEventToTracking(
            kind: .release,
            button: .right,
            event: event
        ) {
            return
        }
        super.rightMouseUp(with: event)
    }

    /// Middle-click pastes from the named selection pasteboard,
    /// mirroring the X11 PRIMARY convention. Works for any
    /// `copy-on-select` mode except `.off` (the pasteboard is empty
    /// in that case so the paste is a no-op). Routes through the same
    /// bracketed-paste-aware path as ⌘V.
    override func otherMouseDown(with event: NSEvent) {
        guard event.buttonNumber == 2 else {
            super.otherMouseDown(with: event)
            return
        }
        guard let s = Self.selectionPasteboard.string(forType: .string),
              !s.isEmpty
        else { return }
        sendBracketedPaste(Data(s.utf8))
    }

    /// Accept middle-click without the view being first responder so a
    /// user can paste from the selection pasteboard into an unfocused
    /// tab without an intermediate click.
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool {
        true
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

    /// Drive the selection from explicit viewport coords (the
    /// `selection.set` IPC op). Mirrors `mouseDown` + `mouseDragged`
    /// but with row/col passed in instead of computed from
    /// `NSEvent.locationInWindow`. Returns `false` and clears the
    /// selection if either point can't convert to a stable screen-y
    /// (out-of-range row, terminal not ready).
    @MainActor
    @discardableResult
    func setSelection(
        anchorCol: Int,
        anchorRow: Int,
        cursorCol: Int,
        cursorRow: Int
    ) -> Bool {
        guard let anchorY = screenY(forViewportRow: anchorRow),
              let cursorY = screenY(forViewportRow: cursorRow)
        else {
            if selection != nil {
                selection = nil
                needsDisplay = true
            }
            return false
        }
        // IPC-driven selections (`selection.set`, the click-count
        // dispatch from `handleClickCount`, the test-mode
        // `tab.expand_selection_at`) are deliberate commits — a
        // single-cell span must still paint + copy. Codex PR #177.
        selection = CellSelection(
            anchorCol: anchorCol,
            anchorScreenY: anchorY,
            cursorCol: cursorCol,
            cursorScreenY: cursorY,
            committed: true
        )
        needsDisplay = true
        return true
    }

    /// Snapshot the current selection for the `selection.dump` IPC op.
    /// Returns `nil` when no selection is active; otherwise carries the
    /// extracted text (same path `⌘C` uses) plus whether each endpoint
    /// is currently visible in the viewport.
    @MainActor
    func dumpSelection() -> SelectionDump? {
        guard let sel = selection else { return nil }
        let text = selectedPlainText()
        let anchorVisible = viewportRow(forScreenY: sel.anchorScreenY) != nil
        let cursorVisible = viewportRow(forScreenY: sel.cursorScreenY) != nil
        return SelectionDump(
            text: text,
            anchorVisible: anchorVisible,
            cursorVisible: cursorVisible
        )
    }

    /// Result of [`dumpSelection`]. Mirrors the `SelectionDumpResult`
    /// wire type. `text` is `nil` when no selection rows are currently
    /// visible (same limitation as `selectedPlainText`).
    struct SelectionDump {
        let text: String?
        let anchorVisible: Bool
        let cursorVisible: Bool
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

    /// Standard responder-chain paste. Text wins first so plain-text
    /// paste behavior is unchanged; image / file-URL fallbacks deliver
    /// a `.png` path to agents like Claude Code and Codex, which
    /// recognise the path and offer to attach. See `PasteImage` for
    /// the extraction order (file URLs → PNG passthrough → AppKit
    /// re-encode). All three branches route through the same
    /// bracketed-paste-aware path so `⌘V` works identically on text,
    /// raw clipboard images, and Finder-copied image files.
    @objc
    func paste(_ sender: Any?) {
        let pb = NSPasteboard.general
        if let s = pb.string(forType: .string), !s.isEmpty {
            sendBracketedPaste(Data(s.utf8))
            return
        }
        switch PasteImage.extract(pb) {
        case .path(let p):
            sendBracketedPaste(Data(p.utf8))
        case .paths(let ps):
            // Newline-separate so a path containing a space (Finder's
            // "Untitled 2.png" or "/Volumes/My Disk/foo.jpg") can't
            // merge with its neighbour. Bracketed paste delivers the
            // bytes verbatim; the receiving agent treats each line as
            // an independent attachment candidate.
            sendBracketedPaste(Data(ps.joined(separator: "\n").utf8))
        case .none:
            return
        }
    }

    /// Wrap `payload` in `ESC[200~ … ESC[201~` when the shell has
    /// DECSET 2004 active and hand it to the input callback. Shared by
    /// `⌘V` (text + image paths) and middle-click PRIMARY paste so the
    /// three paste paths can't drift apart on bracketing or write
    /// routing.
    @MainActor
    private func sendBracketedPaste(_ payload: Data) {
        var bytes = payload
        if bracketedPasteEnabled() {
            // ESC [ 2 0 0 ~ … ESC [ 2 0 1 ~
            var wrapped = Data([0x1b, 0x5b, 0x32, 0x30, 0x30, 0x7e])
            wrapped.append(bytes)
            wrapped.append(contentsOf: [0x1b, 0x5b, 0x32, 0x30, 0x31, 0x7e])
            bytes = wrapped
        }
        onKey?(bytes)
    }

    /// Walk the latest render-state snapshot and concatenate the
    /// glyphs inside the current selection. Trims trailing whitespace
    /// per row + drops empty trailing rows so a multi-line copy
    /// doesn't carry a wall of spaces from cells the terminal hasn't
    /// drawn into. Returns nil when there's no selection.
    ///
    /// Selection rows live in `PointTag::Screen` space; we resolve
    /// each to its current viewport row before walking. Rows that
    /// have scrolled out of the viewport are skipped — copy returns
    /// only the portion of the selection that's currently visible.
    /// A fuller scroll-walk-restore implementation is a follow-up.
    @MainActor
    private func selectedPlainText() -> String? {
        guard let sel = selection else { return nil }
        let n = sel.normalized()
        if let terminal { renderState.update(terminal: terminal) }
        let totalRowSpan = Int(n.endY - n.startY)
        guard totalRowSpan > 0 else { return nil }
        var outRows: [String] = Array(repeating: "", count: totalRowSpan)

        // Map currently-visible viewport rows -> selection offset.
        var offsetForViewportRow: [Int: Int] = [:]
        for offset in 0..<totalRowSpan {
            let screenY = n.startY &+ UInt32(offset)
            if let vRow = viewportRow(forScreenY: screenY) {
                offsetForViewportRow[vRow] = offset
            }
        }
        if offsetForViewportRow.isEmpty { return nil }

        let cols = Int(self.cols)
        renderState.walk { cell in
            guard let offset = offsetForViewportRow[cell.row] else { return }
            let (startCol, endCol) = TerminalView.colRange(
                forOffset: offset,
                totalRowSpan: totalRowSpan,
                normalized: n,
                cols: cols
            )
            guard cell.col >= startCol, cell.col < endCol else { return }
            if let g = cell.glyph {
                outRows[offset].append(String(g))
            } else {
                outRows[offset].append(" ")
            }
        }
        var trimmed = outRows.map {
            String($0.reversed().drop(while: { $0 == " " }).reversed())
        }
        // Drop empty leading rows too — a partial copy where the
        // first selection rows scrolled off-screen leaves their
        // entries as empty strings here, and joining would emit
        // stray leading newlines into the clipboard.
        while let first = trimmed.first, first.isEmpty {
            trimmed.removeFirst()
        }
        while let last = trimmed.last, last.isEmpty {
            trimmed.removeLast()
        }
        return trimmed.joined(separator: "\n")
    }

    /// Compute `[startCol, endCol)` for a single row of a multi-row
    /// selection, given the row's `offset` within the normalized
    /// screen-y range. Single-row selections use the literal cols;
    /// multi-row selections fill the first row from `startCol` to the
    /// right edge, interior rows full-width, and the last row from the
    /// left edge to `endCol`.
    ///
    /// `nonisolated` so the `RoostTests` suite (which is not
    /// `@MainActor`) can exercise it without ceremony. The function
    /// is pure with no shared state, so dropping `@MainActor`
    /// isolation that this otherwise inherits from the enclosing
    /// view class is sound.
    nonisolated static func colRange(
        forOffset offset: Int,
        totalRowSpan: Int,
        normalized n: (startY: UInt32, startCol: Int, endY: UInt32, endCol: Int),
        cols: Int
    ) -> (startCol: Int, endCol: Int) {
        if totalRowSpan == 1 { return (n.startCol, n.endCol) }
        if offset == 0 { return (n.startCol, cols) }
        if offset == totalRowSpan - 1 { return (0, n.endCol) }
        return (0, cols)
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

    /// Post-resolver per-cell snapshot of the full viewport for the
    /// `tab.dump_resolved` IPC op. Each cell carries the same
    /// fg/bg/hasExplicitBg the production paint path computes via
    /// `resolveCellColors(...)` — including the theme's optional
    /// `bold-color` accent. Closes #142's call-site gap: a test
    /// can assert that a bold cell ends up colored by
    /// `theme.boldColor`, which only holds if the production
    /// resolver call site is plumbed correctly.
    struct ResolvedDump {
        let cols: Int
        let rows: Int
        let cells: [ResolvedCell]
    }

    struct ResolvedCell {
        let row: Int
        let col: Int
        /// `" "` for blank cells (matches the Linux `dump_resolved_cells`
        /// shape, so wire-vector parity is byte-exact).
        let text: String
        let foreground: NSColor
        let background: NSColor
        let hasExplicitBg: Bool
        let bold: Bool
        let italic: Bool
        let inverse: Bool
    }

    /// Walk the viewport through the same `resolveCellColors` call
    /// `draw(_:)` runs, including the theme's `boldColor`, for the
    /// `tab.dump_resolved` IPC op. Pins #142's call-site invariant:
    /// the production paint reads `self.theme.boldColor`, so this
    /// op (which reads from the same place) will fail loudly if a
    /// regression sends `nil` past the resolver.
    @MainActor
    func dumpResolvedCells() -> ResolvedDump {
        if let terminal { renderState.update(terminal: terminal) }
        let defaultFg = self.theme.foreground
        let defaultBg = self.theme.background
        let boldColor = self.theme.boldColor
        var cells: [ResolvedCell] = []
        renderState.walk { cell in
            let (fg, bg, hasExplicitBg) = TerminalView.resolveCellColors(
                cell: cell,
                defaultFg: defaultFg,
                defaultBg: defaultBg,
                boldColor: boldColor
            )
            let text: String
            if let g = cell.glyph {
                text = String(g)
            } else {
                text = " "
            }
            cells.append(ResolvedCell(
                row: cell.row,
                col: cell.col,
                text: text,
                foreground: fg,
                background: bg,
                hasExplicitBg: hasExplicitBg,
                bold: cell.bold,
                italic: cell.italic,
                inverse: cell.inverse
            ))
        }
        return ResolvedDump(cols: Int(cols), rows: Int(rows), cells: cells)
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
        return isDecModeEnabled(2004)
    }

    /// Read a DEC private mode flag. `0x7FFF` mask is the bit packing
    /// the `ghostty_mode_new` C macro uses: low 15 bits = mode number,
    /// bit 15 = ANSI flag (always 0 for DEC private modes). Returns
    /// `false` on any FFI hiccup so callers default to "not enabled."
    @MainActor
    private func isDecModeEnabled(_ mode: UInt16) -> Bool {
        guard let terminal else { return false }
        var on = false
        let m = GhosttyMode(mode & 0x7FFF)
        let rc = ghostty_terminal_mode_get(terminal, m, &on)
        return rc.rawValue == 0 && on
    }

    /// True when mode 1003 (any-event motion tracking) is on. Strix
    /// enables this so it can detect hover over its split bar without
    /// holding a button. Without 1003, only mode 1000/1002 reports
    /// are emitted and motion-no-button is suppressed at the call
    /// site.
    @MainActor
    private func isAnyEventMotionTrackingActive() -> Bool {
        return isDecModeEnabled(1003)
    }

    /// True when mode 1004 (focus tracking) is on. vim, less, btop
    /// and similar TUIs use it to redraw on focus state changes.
    @MainActor
    private func isFocusTrackingActive() -> Bool {
        return isDecModeEnabled(1004)
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
    ///   1. Mouse-tracking mode → mouse-button-4/5 reports encoded
    ///      through the libghostty-vt mouse encoder, one per row, at
    ///      the pointer's cell. Checked first so a mouse-tracking
    ///      alt-screen app (htop) gets the report, not arrow keys.
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

    /// Decide via the pure `computeMouseTrackingDispatch` whether to
    /// forward this mouse event to the encoder, and (if so) call
    /// the encoder + emit. Returns `true` when the event was
    /// consumed by the mouse-tracking forward — caller must not
    /// run any other handler. Returns `false` when the event should
    /// fall through to selection / paste / etc.
    ///
    /// `linkClickConsumedThisGesture` already short-circuits Cmd-click
    /// URL launches at the very top of each NSEvent override, so the
    /// `urlInterceptsClick` argument here is `false` — by the time we
    /// reach this helper, any URL precedence has already been applied.
    @MainActor
    private func forwardMouseEventToTracking(
        kind: MouseRoutingAction,
        button: MouseRoutingButton?,
        event: NSEvent
    ) -> Bool {
        let dispatch = computeMouseTrackingDispatch(
            eventKind: kind,
            button: button,
            isMouseTrackingActive: isMouseTrackingActive(),
            urlInterceptsClick: false
        )
        guard case .forward(let action, let buttonOut) = dispatch else {
            return false
        }
        let p = convert(event.locationInWindow, from: nil)
        return emitMouseTracking(
            action: action,
            button: buttonOut,
            mods: Self.mouseMods(forFlags: event.modifierFlags),
            point: p
        )
    }

    /// Drive the encoder and emit. Lifted out of
    /// `forwardMouseEventToTracking` so the test-mode IPC op
    /// (`tab.dispatch_mouse_event`) can call it with synthetic
    /// cell coordinates instead of an `NSEvent`.
    ///
    /// For motion-no-button (mode 1003) we run `motionEmitter` here
    /// so the throttle applies to BOTH the real `mouseMoved` path
    /// AND the IPC test seam. Without this, a sweep that takes the
    /// real cursor 16 ms per cell would emit at ~60 Hz but the
    /// synthetic IPC dispatch would emit one report per call,
    /// blowing past the cap.
    @MainActor
    @discardableResult
    func emitMouseTracking(
        action: MouseRoutingAction,
        button: MouseRoutingButton?,
        mods: GhosttyMods,
        point: NSPoint
    ) -> Bool {
        guard let mouseEncoder else { return false }
        let cw = max(cellSize.width, 1)
        let ch = max(cellSize.height, 1)
        // Clamp the point into the grid so an event just off the
        // edge still names the last cell, not an out-of-range
        // coordinate. Mirrors `scrollWheel`'s clamp.
        let x = Float(min(max(point.x, 0), cw * CGFloat(cols) - 1))
        let y = Float(min(max(point.y, 0), ch * CGFloat(rows) - 1))
        let screenW = UInt32(cw * CGFloat(cols))
        let screenH = UInt32(ch * CGFloat(rows))
        let cellWPx = UInt32(cw)
        let cellHPx = UInt32(ch)
        // 60 Hz cap + per-cell dedup for motion-no-button. Press /
        // release / drag (motion-with-button) are intentionally NOT
        // throttled: TUIs care about every press and release event,
        // and drag motion under mode 1002 lives or dies on tight
        // feedback. The throttle exists only because mode 1003 floods
        // the input drain when the user sweeps the pointer fast.
        //
        // PEEK now; COMMIT only if the encoder actually emits below.
        // Committing on a declined encode would silently suppress
        // the next event the encoder would have accepted — e.g. mode
        // 1003 toggling on between two same-cell motions.
        //
        // The throttle keys on the CLAMPED cell (derived from `x`/`y`,
        // not the raw `point`) so an off-grid event that the encoder
        // sees as the edge cell also dedups against the same key.
        let motionCellCol: Int
        let motionCellRow: Int
        let motionNow: Double
        let throttleMotionNoButton = (action == .motion && button == nil)
        if throttleMotionNoButton {
            motionCellCol = Int(CGFloat(x) / cw)
            motionCellRow = Int(CGFloat(y) / ch)
            motionNow = CACurrentMediaTime()
            guard motionEmitter.wouldEmit(
                col: motionCellCol,
                row: motionCellRow,
                nowSeconds: motionNow
            ) else {
                return false
            }
        } else {
            motionCellCol = 0
            motionCellRow = 0
            motionNow = 0
        }

        // Motion with `button: nil` → mode 1003 any-motion path.
        // Everything else (press / release / motion-with-button)
        // takes the explicit-button path.
        let bytes: Data
        if action == .motion, button == nil {
            bytes = mouseEncoder.encodeMotionNoButton(
                mods: mods,
                x: x, y: y,
                screenWidth: screenW, screenHeight: screenH,
                cellWidth: cellWPx, cellHeight: cellHPx
            )
        } else {
            guard let b = button else { return false }
            bytes = mouseEncoder.encodeButton(
                action: Self.cMouseAction(forRoutingAction: action),
                button: Self.cMouseButton(forRoutingButton: b),
                mods: mods,
                x: x, y: y,
                screenWidth: screenW, screenHeight: screenH,
                cellWidth: cellWPx, cellHeight: cellHPx
            )
        }
        if bytes.isEmpty {
            // Encoder declined (negotiated mode doesn't permit this
            // report — e.g. mode 1000 receives a motion event). Don't
            // swallow the event AND don't commit the throttle: the
            // next call should be allowed to retry if the mode flips.
            return false
        }
        if throttleMotionNoButton {
            motionEmitter.commit(
                col: motionCellCol,
                row: motionCellRow,
                nowSeconds: motionNow
            )
        }
        onKey?(bytes)
        return true
    }

    private static func cMouseAction(
        forRoutingAction action: MouseRoutingAction
    ) -> GhosttyMouseAction {
        switch action {
        case .press: return GHOSTTY_MOUSE_ACTION_PRESS
        case .release: return GHOSTTY_MOUSE_ACTION_RELEASE
        case .motion: return GHOSTTY_MOUSE_ACTION_MOTION
        }
    }

    private static func cMouseButton(
        forRoutingButton button: MouseRoutingButton
    ) -> GhosttyMouseButton {
        switch button {
        case .left: return GHOSTTY_MOUSE_BUTTON_LEFT
        case .right: return GHOSTTY_MOUSE_BUTTON_RIGHT
        case .middle: return GHOSTTY_MOUSE_BUTTON_MIDDLE
        case .four: return GHOSTTY_MOUSE_BUTTON_FOUR
        case .five: return GHOSTTY_MOUSE_BUTTON_FIVE
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
        //
        // **Two-pass walk** — collect bg fills + glyph draws in one
        // walk, then paint Pass A (all bg rects) and Pass B (all
        // glyphs) separately. Pre-split this was a single per-cell
        // loop; a descender from row N (e.g. the lower stem of a 'g'
        // in a gray prompt cell) could be painted, then row N+1's
        // bg fill would clobber the descender ink because the loop
        // walked in row-major order. Linux already does this same
        // split — see `crates/roost-linux/src/terminal_view.rs` Pass
        // A/B comments. SGR style bits (especially `inverse`) are
        // applied via `resolveCellColors` so codex's `\e[7m` prompt
        // row renders its gray bg.
        //
        // Glyph drawing currently uses NSAttributedString.draw —
        // simple, slow-but-correct. A glyph atlas (Core Text +
        // CGContextShowGlyphsAtPositions) is the next-tier
        // optimization once StreamPty starts pushing frames at
        // human-typing rates and per-cell allocations matter.
        let cellW = cellSize.width
        let cellH = cellSize.height
        let defaultFg = canvasFg
        let defaultBg = canvasBg
        let cellFont = self.font

        struct BgFill { let rect: NSRect; let color: NSColor }
        struct GlyphDraw {
            let glyph: Character
            let foreground: NSColor
            let origin: NSPoint
        }

        var bgFills: [BgFill] = []
        var glyphDraws: [GlyphDraw] = []
        let boldColor = self.theme.boldColor

        renderState.walk { cell in
            let (fg, bg, hasExplicitBg) = TerminalView.resolveCellColors(
                cell: cell,
                defaultFg: defaultFg,
                defaultBg: defaultBg,
                boldColor: boldColor
            )
            let rect = NSRect(
                x: CGFloat(cell.col) * cellW,
                y: CGFloat(cell.row) * cellH,
                width: cellW,
                height: cellH
            )
            if hasExplicitBg {
                bgFills.append(BgFill(rect: rect, color: bg))
            }
            if let glyph = cell.glyph, !glyph.isWhitespace {
                glyphDraws.append(GlyphDraw(
                    glyph: glyph,
                    foreground: fg,
                    // Bottom-align glyphs to the cell's baseline.
                    // The grid origin is top-left (isFlipped=true),
                    // so the glyph's drawing origin is at the cell
                    // top + the font's ascender.
                    origin: NSPoint(x: rect.minX, y: rect.minY)
                ))
            }
            // Stash glyph at the cursor's cell so the cursor pass can
            // redraw it in an inverted color over a focused block
            // cursor. Done inline so we don't need a second walk.
            if let cur = cursorInfo,
               cell.row == Int(cur.row),
               cell.col == Int(cur.col)
            {
                self.cursorCellGlyph = cell.glyph
            }
        }

        // Pass A — backgrounds.
        for fill in bgFills {
            fill.color.setFill()
            fill.rect.fill()
        }

        // Pass B — glyphs. Box-drawing (U+2500..U+257F) and block-
        // element (U+2580..U+259F) codepoints get a custom geometric
        // renderer (`Sprite.draw`) that tiles pixel-perfectly across
        // cells; everything else falls through to NSAttributedString.
        // Core Text fonts produce visible seams in TUI chrome —
        // most obvious in the opencode wordmark logo — which is what
        // the Sprite module exists to fix.
        let cgCtx = NSGraphicsContext.current?.cgContext
        for draw in glyphDraws {
            // Sprite-render single-scalar grapheme cells whose
            // codepoint falls in one of the geometric ranges.
            // Multi-scalar graphemes (emoji ZWJ, combining marks)
            // skip this path because the sprite layer is
            // by-codepoint, not by-grapheme.
            let scalars = draw.glyph.unicodeScalars
            if let cgCtx,
               scalars.count == 1,
               let scalar = scalars.first,
               Sprite.draw(
                   in: cgCtx,
                   x: draw.origin.x,
                   y: draw.origin.y,
                   w: cellW,
                   h: cellH,
                   fg: draw.foreground,
                   codepoint: scalar.value
               )
            {
                continue
            }
            let attrs: [NSAttributedString.Key: Any] = [
                .font: cellFont,
                .foregroundColor: draw.foreground,
            ]
            let line = NSAttributedString(string: String(draw.glyph), attributes: attrs)
            line.draw(at: draw.origin)
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

        // Clickable-link underline (PR B). Draw a single-pixel rule
        // across the bottom of the hovered URL's cells when Cmd is
        // held + the pointer is over a URL. Sits between glyph and
        // selection overlay so an active drag-selection over a URL
        // still wins visually. Color is `theme.foreground` for v1 —
        // a dedicated `link-color` theme key is a future widening.
        if let hov = hoverURL, commandHeld {
            let underline = NSRect(
                x: CGFloat(hov.col0) * cellW,
                y: CGFloat(hov.row + 1) * cellH - 1,
                width: CGFloat(hov.col1 - hov.col0 + 1) * cellW,
                height: 1
            )
            theme.foreground.setFill()
            underline.fill()
        }

        // Selection overlay (Phase 6a M5). Drawn last so it sits on
        // top of the glyph pass — translucent accent fill, no border.
        //
        // Selection rows are stored in screen-y (scrollback-stable)
        // space; resolve each to a viewport row before drawing so the
        // highlight scrolls with the content. Rows currently outside
        // the visible viewport are skipped — the rectangle "exits"
        // off the top / bottom of the view as the user scrolls.
        if let sel = selection, sel.isVisible {
            let n = sel.normalized()
            let overlay = theme.selectionBackground.withAlphaComponent(0.6)
            overlay.setFill()
            let totalRowSpan = Int(n.endY - n.startY)
            let colsInt = Int(cols)
            for offset in 0..<totalRowSpan {
                let screenY = n.startY &+ UInt32(offset)
                guard let vRow = viewportRow(forScreenY: screenY) else { continue }
                let (startCol, endCol) = TerminalView.colRange(
                    forOffset: offset,
                    totalRowSpan: totalRowSpan,
                    normalized: n,
                    cols: colsInt
                )
                let r = NSRect(
                    x: CGFloat(startCol) * cellW,
                    y: CGFloat(vRow) * cellH,
                    width: CGFloat(endCol - startCol) * cellW,
                    height: cellH
                )
                r.fill()
            }
        }
    }

    // MARK: - Cursor draw helpers (M2)

    /// Resolve a cell's effective fg/bg + whether it needs a BG fill,
    /// applying SGR inverse + bold-accent rules. Static so it's pure
    /// (no `self`) and can be unit-tested in
    /// `mac/Tests/RoostTests/RenderResolverTests.swift`.
    ///
    /// Mirrors the legacy Go `cellColors`
    /// (`cmd/roost/render.go:206-224`) and the Rust
    /// `resolve_cell_colors` (`crates/roost-linux/src/terminal_view.rs`)
    /// 1:1 — same rule order (explicit-color lookup → inverse swap →
    /// bold-accent guarded by `!inverse && fg-was-default`) so both
    /// UIs behave identically on inverse-marked TUI chrome (codex's
    /// gray prompt row) and bold default-fg text.
    ///
    /// `boldColor` comes from `Theme.boldColor`, populated from the
    /// Ghostty `bold-color` key. Themes that omit it pass `nil` and
    /// bold default-fg cells render in the canvas fg.
    ///
    /// `nonisolated` because the function is pure (no `self`, no
    /// global state). Without it, Swift 6 strict concurrency
    /// inherits `@MainActor` from the enclosing `TerminalView` and
    /// the swift-testing `@Test` functions (which run on the
    /// testing-library's own executor) can't call it synchronously.
    nonisolated static func resolveCellColors(
        cell: RenderState.Cell,
        defaultFg: NSColor,
        defaultBg: NSColor,
        boldColor: NSColor?
    ) -> (foreground: NSColor, background: NSColor, hasExplicitBg: Bool) {
        var fg = cell.foreground ?? defaultFg
        var bg = cell.background ?? defaultBg
        var hasExplicitBg = cell.background != nil
        if cell.inverse {
            (fg, bg) = (bg, fg)
            hasExplicitBg = true
        }
        if cell.bold && cell.foreground == nil && !cell.inverse {
            if let bc = boldColor {
                fg = bc
            }
        }
        return (fg, bg, hasExplicitBg)
    }

    /// Synthesise the XTerm-form OSC 10/11/12 query response for
    /// the given query number + theme color. Byte-identical to the
    /// Rust `format_color_query_response` in
    /// `crates/roost-osc/src/lib.rs` — both ports must produce the
    /// same bytes so codex/claude-code see one terminal answer
    /// regardless of which UI hosts the tab. `nonisolated` because
    /// the function is pure (no `self`); see `resolveCellColors`
    /// above for the same Swift 6 strict-concurrency rationale.
    ///
    /// Returns `nil` if `n` isn't one of the recognised color-query
    /// numbers (10, 11, 12). Returns `nil` when the color can't be
    /// converted to sRGB components (defensive — every bundled theme
    /// color does convert).
    nonisolated static func formatColorQueryResponse(n: UInt8, color: NSColor) -> Data? {
        guard (10...12).contains(n), let srgb = color.usingColorSpace(.sRGB) else {
            return nil
        }
        let r = UInt8(round(srgb.redComponent * 255))
        let g = UInt8(round(srgb.greenComponent * 255))
        let b = UInt8(round(srgb.blueComponent * 255))
        // 16-bit-per-channel form: each 8-bit channel repeated to
        // fill 4 hex digits, BEL-terminated. Matches xterm's reply
        // and what codex/claude expect.
        let s = String(
            format: "\u{1B}]%d;rgb:%02x%02x/%02x%02x/%02x%02x\u{07}",
            Int(n), r, r, g, g, b, b
        )
        return Data(s.utf8)
    }

    /// Read the live effective color libghostty would render with for
    /// the given OSC color-query number (10=fg, 11=bg, 12=cursor),
    /// falling back to the theme when libghostty hasn't tracked a
    /// value yet. Centralised so the OSC reply path on the Mac
    /// matches the Linux `TerminalView::live_colors` shape — both
    /// UIs must reply with the same color a `vim`-driven
    /// `OSC 11;rgb:…` set most recently established.
    ///
    /// Returns `nil` if `n` isn't 10/11/12.
    @MainActor
    static func liveColor(
        forQuery n: UInt8,
        terminal: GhosttyTerminal,
        theme: Theme
    ) -> NSColor? {
        let dataKey: GhosttyTerminalData
        let themeFallback: NSColor
        switch n {
        case 10:
            dataKey = GHOSTTY_TERMINAL_DATA_COLOR_FOREGROUND
            themeFallback = theme.foreground
        case 11:
            dataKey = GHOSTTY_TERMINAL_DATA_COLOR_BACKGROUND
            themeFallback = theme.background
        case 12:
            dataKey = GHOSTTY_TERMINAL_DATA_COLOR_CURSOR
            themeFallback = theme.cursor
        default:
            return nil
        }
        var rgb = GhosttyColorRgb(r: 0, g: 0, b: 0)
        let rc = ghostty_terminal_get(terminal, dataKey, &rgb)
        guard rc.rawValue == 0 else {
            // GHOSTTY_NO_VALUE (or any other non-zero rc) means
            // libghostty isn't reporting a default yet — render with
            // the theme, which is what the renderer paints anyway.
            return themeFallback
        }
        return NSColor(
            srgbRed: CGFloat(rgb.r) / 255,
            green: CGFloat(rgb.g) / 255,
            blue: CGFloat(rgb.b) / 255,
            alpha: 1
        )
    }

    /// Solid block cursor with the underlying glyph re-painted in an
    /// inverted color so the character stays legible. Uses
    /// `theme.background` as the inverted-text color — fine for the
    /// fallback + bundled themes; if a theme ever ships a dedicated
    /// `cursorText` field we'd thread it through instead.
    private func drawCursorBlock(in rect: NSRect, color: NSColor, cellFont: NSFont) {
        color.setFill()
        rect.fill()
        guard let glyph = cursorCellGlyph, !glyph.isWhitespace else { return }
        // Same sprite-vs-Pango dispatch as Pass B — a block-element
        // cursor cell (e.g. a ▌ under a TUI cursor) must redraw
        // geometrically too or it'd seam against the cursor block.
        let scalars = glyph.unicodeScalars
        if let cgCtx = NSGraphicsContext.current?.cgContext,
           scalars.count == 1,
           let scalar = scalars.first,
           Sprite.draw(
               in: cgCtx,
               x: rect.minX,
               y: rect.minY,
               w: rect.width,
               h: rect.height,
               fg: theme.background,
               codepoint: scalar.value
           )
        {
            return
        }
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
