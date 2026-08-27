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
// merely documented. Selection endpoints live outside as libghostty
// *tracked* refs, which the engine keeps current; snapshotting them into
// raw pins happens here, immediately before the `GhosttySelection` is
// built, and the pins die with the call.

import CGhosttyVT
import Foundation

enum SelectionFormatter {
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
            options.unwrap = unwrapSoftWrappedLines
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
