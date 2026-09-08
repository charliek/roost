// Text extraction via `ghostty_terminal_selection_format_alloc`.
//
// The formatter is the only libghostty API that can read cells outside
// the viewport, so it — not the render state — is what makes a
// scrollback-spanning copy, and a scrollback-carrying dump, complete.
// Swift mirror of `crates/roost-vt/src/formatter.rs`; the two must stay
// behaviorally identical.
//
// # Why every reader here formats inside one call
//
// `GhosttyGridRef` is an unvalidated pin into the terminal's page list,
// and libghostty resolves a selection's endpoints with an unchecked
// `pointFromPin(...).?` (`Selection.order`). The archive is built
// `-Doptimize=ReleaseFast`, where that null unwrap is undefined
// behavior rather than a panic. Upstream codifies this as an unchecked
// precondition rather than validating it, so any mutating terminal call
// — `vt_write`, `resize`, `reset`, an alt-screen switch — landing
// between the pin and the format is enough to trigger it.
//
// Every reader here therefore pins, formats, and frees inside a single
// synchronous call. No grid ref escapes them, which makes the hazardous
// interleaving unrepresentable instead of merely documented. Selection
// endpoints live outside as libghostty *tracked* refs, which the engine
// keeps current; snapshotting them into raw pins happens here,
// immediately before the `GhosttySelection` is built, and the pins die
// with the call.

import CGhosttyVT
import Foundation

enum SelectionFormatter {
    /// A libghostty call this module's arithmetic says cannot fail did.
    ///
    /// Thrown rather than folded into an empty result: history that came
    /// back short because a read failed is indistinguishable from
    /// history that is short, and `tab.dump`'s caller would silently
    /// renumber it. The IPC handler maps this to `internal`.
    enum FormatError: Error, CustomStringConvertible {
        case read(String)

        var description: String {
            switch self {
            case .read(let message): return message
            }
        }
    }

    /// Join soft-wrapped rows into one line when copying.
    ///
    /// Plan 024 D4.4. This is a **deliberate, visible behavior change**:
    /// a line the terminal wrapped across several rows copies as one
    /// long line, the way Ghostty and every other modern terminal copy
    /// it, instead of as one line per screen row. Flip it to `false` to
    /// restore per-row copying.
    ///
    /// Both copy paths honor this constant — libghostty's formatter
    /// here, and `TerminalView.viewportSelectedText`'s render-state walk
    /// that handles a selection entirely inside the viewport — so the
    /// two agree whichever way it is set, and a copy never depends on
    /// scroll position. Its Rust twin is
    /// `roost_vt::UNWRAP_SOFT_WRAPPED_LINES`; the two must match or the
    /// Mac and Linux UIs copy differently.
    static let unwrapSoftWrappedLines = true

    /// Format the inclusive cell range `start...end` of the active
    /// screen as plain text.
    ///
    /// Both endpoints are inclusive — pass the raw anchor/cursor cells,
    /// not a half-open range. Drag order does not matter: libghostty
    /// normalizes reversed endpoints itself via `Selection.order`.
    ///
    /// Returns `nil` when an endpoint no longer names a cell — a row
    /// evicted from scrollback, or a terminal reset. That is an empty
    /// selection, not a failure.
    ///
    /// Both endpoints must belong to the terminal's currently active
    /// screen; the caller gates on that, because libghostty's formatter
    /// treats it as a precondition.
    @MainActor
    static func text(
        terminal: GhosttyTerminal,
        start: GhosttyTrackedGridRef,
        end: GhosttyTrackedGridRef
    ) -> String? {
        guard let startRef = snapshot(start), let endRef = snapshot(end) else { return nil }
        return try? formatSelection(
            terminal: terminal, start: startRef, end: endRef, unwrap: unwrapSoftWrappedLines
        )
    }

    /// Rows of history sitting above the **current** viewport.
    ///
    /// Deliberately the scrollbar's `offset` rather than `total - len`:
    /// history is anchored on what the viewport is showing, so a caller
    /// reading a scrolled terminal gets the rows adjacent to its own
    /// first row instead of rows it is already displaying.
    @MainActor
    static func scrollbackRows(terminal: GhosttyTerminal) throws -> UInt32 {
        var out = GhosttyTerminalScrollbar()
        guard
            ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_SCROLLBAR, &out)
                == GHOSTTY_SUCCESS
        else { throw FormatError.read("read scrollbar: ghostty_terminal_get failed") }
        return UInt32(clamping: out.offset)
    }

    /// The last `rows` lines of history above the current viewport, top
    /// to bottom — the final entry is always the row immediately above
    /// the viewport's first row.
    ///
    /// Fewer rows than asked for only when history is shorter; the
    /// result is exactly `min(rows, scrollbackRows)` entries, blank rows
    /// included as empty strings.
    @MainActor
    static func scrollbackText(terminal: GhosttyTerminal, rows: UInt32) throws -> [String] {
        let top = try scrollbackRows(terminal: terminal)
        let count = min(rows, top)
        if count == 0 { return [] }
        guard let cols = columns(terminal) else {
            throw FormatError.read("read cols: ghostty_terminal_get failed")
        }

        // Every history row is `cols` wide, so a rejected endpoint is a
        // bug in this arithmetic, never a state history can be in —
        // reporting it as empty history would hide it.
        guard let start = gridRef(terminal, col: 0, screenY: top - count),
              let end = gridRef(terminal, col: cols > 0 ? cols - 1 : 0, screenY: top - 1)
        else {
            throw FormatError.read(
                "history rows \(top - count)…\(top - 1) have no grid ref"
            )
        }

        // Not unwrapped, whatever the soft-wrap flag says: the caller
        // indexes history by row.
        let text = try formatSelection(
            terminal: terminal, start: start, end: end, unwrap: false
        )

        // `components(separatedBy:)`, not `split`, which drops empty
        // subsequences — a blank row inside the range is an empty
        // string that has to keep its position.
        var lines = text.components(separatedBy: "\n")
        // The formatter emits a row's newline only when a later
        // non-blank row follows, so blank rows at the end of the range
        // come back missing rather than empty. Pad them back: the count
        // is part of the contract, and a short array would silently
        // renumber history.
        while lines.count < Int(count) { lines.append("") }
        return lines
    }

    /// The one `selection_format_alloc` call this module's readers
    /// share: `start...end` as plain text with trailing spaces removed,
    /// soft-wrapped rows joined when `unwrap`.
    ///
    /// Both pins must be taken immediately before the call with nothing
    /// touching the terminal in between, for the reason this file's
    /// header gives.
    @MainActor
    private static func formatSelection(
        terminal: GhosttyTerminal,
        start: GhosttyGridRef,
        end: GhosttyGridRef,
        unwrap: Bool
    ) throws -> String {
        var selection = GhosttySelection()
        selection.size = MemoryLayout<GhosttySelection>.size
        selection.start = start
        selection.end = end
        selection.rectangle = false

        // The selection pointer only has to outlive the one
        // `ghostty_terminal_selection_format_alloc` call, but it is kept
        // valid for the whole body anyway.
        return try withUnsafePointer(to: &selection) { selectionPtr -> String in
            var options = GhosttyTerminalSelectionFormatOptions()
            options.size = MemoryLayout<GhosttyTerminalSelectionFormatOptions>.size
            options.emit = GHOSTTY_FORMATTER_FORMAT_PLAIN
            options.unwrap = unwrap
            // Roost does want trailing spaces gone, but not
            // libghostty's version of it: its trim treats any cell
            // whose base codepoint is a space as blank, so a space
            // carrying a combining mark loses the mark and comes back
            // as a bare space. Trailing spaces are removed in
            // `trimTrailingSpaces` instead, which is otherwise
            // equivalent — textless cells are dropped either way.
            options.trim = false
            // Non-null, so the terminal's own active selection is not
            // consulted and `GHOSTTY_NO_VALUE` cannot come back for a
            // missing one.
            options.selection = selectionPtr

            var outPtr: UnsafeMutablePointer<UInt8>?
            var outLen = 0
            let rc = ghostty_terminal_selection_format_alloc(
                terminal, nil, options, &outPtr, &outLen
            )
            guard rc == GHOSTTY_SUCCESS else {
                throw FormatError.read(
                    "ghostty_terminal_selection_format_alloc failed (rc=\(rc.rawValue))"
                )
            }
            guard let outPtr else { return "" }
            // Freed with the same (null) default allocator and the exact
            // length the call reported, as its contract requires.
            defer { ghostty_free(nil, outPtr, outLen) }

            let bytes = UnsafeBufferPointer(start: outPtr, count: outLen)
            guard let raw = String(bytes: bytes, encoding: .utf8) else {
                throw FormatError.read("formatter output is not UTF-8")
            }
            return trimTrailingSpaces(raw)
        }
    }

    /// Drop trailing `U+0020` from every line. Only spaces — every
    /// other whitespace codepoint is content a terminal cell holds
    /// deliberately. Operates on Unicode scalars, not `Character`s, so
    /// a `\r\n` pair (one Swift `Character`) still splits on its `\n`
    /// and a space carrying a combining mark is not mistaken for a
    /// bare space.
    ///
    /// Shared with the viewport walk so both paths trim identically.
    /// With `unwrapSoftWrappedLines` on, "line" means the joined logical
    /// line: a wrapped row's trailing spaces sit mid-line and survive,
    /// which is what keeps the rejoin from eating characters.
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

    /// Pin a screen-space cell. `nil` when libghostty rejects the point.
    ///
    /// Private, and called only from `scrollbackText` above, for the
    /// same reason `snapshot` is private: the pin dies with the one
    /// synchronous call that formats with it.
    @MainActor
    private static func gridRef(
        _ terminal: GhosttyTerminal,
        col: UInt16,
        screenY: UInt32
    ) -> GhosttyGridRef? {
        var point = GhosttyPoint()
        point.tag = GHOSTTY_POINT_TAG_SCREEN
        point.value.coordinate.x = col
        point.value.coordinate.y = screenY
        var ref = GhosttyGridRef()
        ref.size = MemoryLayout<GhosttyGridRef>.size
        guard ghostty_terminal_grid_ref(terminal, point, &ref) == GHOSTTY_SUCCESS,
              ref.node != nil
        else { return nil }
        return ref
    }

    /// The terminal's width in cells. `nil` on an FFI failure.
    @MainActor
    private static func columns(_ terminal: GhosttyTerminal) -> UInt16? {
        var out: UInt16 = 0
        guard
            ghostty_terminal_get(terminal, GHOSTTY_TERMINAL_DATA_COLS, &out) == GHOSTTY_SUCCESS
        else { return nil }
        return out
    }

    /// Snapshot a tracked ref into an untracked pin. `nil` when the
    /// tracked content was discarded (`GHOSTTY_NO_VALUE`).
    ///
    /// Private, and called only from `text` above: the pin is valid
    /// only until the terminal's next update, so it must not outlive
    /// the one synchronous call that formats with it.
    @MainActor
    private static func snapshot(_ tracked: GhosttyTrackedGridRef) -> GhosttyGridRef? {
        var ref = GhosttyGridRef()
        ref.size = MemoryLayout<GhosttyGridRef>.size
        guard ghostty_tracked_grid_ref_snapshot(tracked, &ref) == GHOSTTY_SUCCESS,
              ref.node != nil
        else { return nil }
        return ref
    }
}
