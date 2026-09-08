// Contract battery for the history readers `SelectionFormatter
// .scrollbackRows` / `.scrollbackText` — what `tab.dump`'s scrollback
// fields are made of on the Mac.
//
// Swift mirror of `crates/roost-vt/tests/scrollback_text_test.rs`; the
// cases are deliberately the same ones so a divergence between the Mac
// and Linux UIs shows up as a failing test on one side.
//
// Three properties here are what a caller stitching history onto a
// viewport depends on. The **anchor**: history is measured from the
// current viewport, so the last row returned is always the one
// immediately above the viewport's first row, scrolled or not. The
// **count**: exactly `min(rows, scrollbackRows)` entries come back,
// blank rows included — libghostty's formatter drops trailing blank rows
// entirely, so a reader that forwarded its output unpadded would
// renumber history without saying so. And the **trim**: rows come back
// byte-identical to what a copy of the same row produces, including a
// trailing space that carries a combining mark.

import CGhosttyVT
import Testing

@testable import Roost

private let historyCols: UInt16 = 80
private let historyRows: UInt16 = 24

@MainActor
private func makeTerminal() throws -> GhosttyTerminal {
    var maybe: GhosttyTerminal?
    #expect(ghostty_terminal_new(nil, &maybe, historyCols, historyRows).rawValue == 0)
    let terminal = try #require(maybe, "ghostty_terminal_new returned success but term is nil")
    setScrollbackLines(terminal, 2000)
    return terminal
}

@MainActor
private func write(_ terminal: GhosttyTerminal, _ text: String) {
    let bytes = Array(text.utf8)
    bytes.withUnsafeBufferPointer {
        ghostty_terminal_vt_write(terminal, $0.baseAddress, bytes.count)
    }
}

/// Numbered lines, each on its own row, cursor left on a fresh row.
@MainActor
private func writeLines(_ terminal: GhosttyTerminal, _ numbers: ClosedRange<Int>) {
    for n in numbers { write(terminal, "line \(n)\r\n") }
}

private func numbered(_ numbers: ClosedRange<Int>) -> [String] {
    numbers.map { "line \($0)" }
}

@MainActor
private func history(_ terminal: GhosttyTerminal, _ rows: UInt32) throws -> [String] {
    try SelectionFormatter.scrollbackText(terminal: terminal, rows: rows)
}

@MainActor
private func historyLength(_ terminal: GhosttyTerminal) throws -> UInt32 {
    try SelectionFormatter.scrollbackRows(terminal: terminal)
}

/// The viewport's first row, read through `SelectionFormatter.text` in
/// *viewport* coordinates — an independent cross-check that history's
/// last row really is the one directly above it, since the reader under
/// test works in screen coordinates.
@MainActor
private func firstViewportRow(_ terminal: GhosttyTerminal) -> String? {
    guard let start = trackedViewportRef(terminal, col: 0),
          let end = trackedViewportRef(terminal, col: historyCols - 1)
    else { return nil }
    defer {
        ghostty_tracked_grid_ref_free(start)
        ghostty_tracked_grid_ref_free(end)
    }
    return SelectionFormatter.text(terminal: terminal, start: start, end: end)
}

@MainActor
private func trackedViewportRef(
    _ terminal: GhosttyTerminal,
    col: UInt16
) -> GhosttyTrackedGridRef? {
    var point = GhosttyPoint()
    point.tag = GHOSTTY_POINT_TAG_VIEWPORT
    point.value.coordinate.x = col
    point.value.coordinate.y = 0
    var ref: GhosttyTrackedGridRef?
    guard ghostty_terminal_grid_ref_track(terminal, point, &ref) == GHOSTTY_SUCCESS,
          let ref
    else { return nil }
    return ref
}

@MainActor
private func scrollViewport(_ terminal: GhosttyTerminal, delta: Int) {
    var behavior = GhosttyTerminalScrollViewport()
    behavior.tag = GHOSTTY_SCROLL_VIEWPORT_DELTA
    behavior.value.delta = delta
    ghostty_terminal_scroll_viewport(terminal, behavior)
}

@MainActor
private func scrollViewportToTop(_ terminal: GhosttyTerminal) {
    var behavior = GhosttyTerminalScrollViewport()
    behavior.tag = GHOSTTY_SCROLL_VIEWPORT_TOP
    ghostty_terminal_scroll_viewport(terminal, behavior)
}

@Test @MainActor
func historyIsContiguousAndEndsOnTheRowAboveTheViewport() throws {
    let terminal = try makeTerminal()
    defer { ghostty_terminal_free(terminal) }
    writeLines(terminal, 1...100)

    let rows = try history(terminal, 50)
    #expect(rows.count == 50)
    #expect(rows == numbered(28...77), "contiguous, top to bottom")
    #expect(
        rows.last == "line 77",
        "the last row of history sits directly above the viewport"
    )
    #expect(firstViewportRow(terminal) == "line 78")
}

@Test @MainActor
func aRequestPastTheHistoryReturnsEveryRowThereIs() throws {
    let terminal = try makeTerminal()
    defer { ghostty_terminal_free(terminal) }
    writeLines(terminal, 1...100)

    let all = try history(terminal, 10_000)
    let length = try historyLength(terminal)
    #expect(UInt32(all.count) == length)
    #expect(all == numbered(1...77))
}

@Test @MainActor
func aFreshTerminalHasNoHistory() throws {
    let terminal = try makeTerminal()
    defer { ghostty_terminal_free(terminal) }

    let length = try historyLength(terminal)
    let rows = try history(terminal, 50)
    #expect(length == 0)
    #expect(rows == [])
}

@Test @MainActor
func theAlternateScreenHasNoHistory() throws {
    let terminal = try makeTerminal()
    defer { ghostty_terminal_free(terminal) }
    writeLines(terminal, 1...100)
    write(terminal, "\u{1b}[?1049h")

    let length = try historyLength(terminal)
    let rows = try history(terminal, 50)
    #expect(length == 0)
    #expect(rows == [])
}

/// Blank rows at the bottom of the requested range are the case
/// libghostty's formatter drops on the floor: it defers a row's newline
/// until a later non-blank row, so these come back as nothing at all.
@Test @MainActor
func blankRowsAtTheBottomOfHistoryComeBackAsEmptyStrings() throws {
    let terminal = try makeTerminal()
    defer { ghostty_terminal_free(terminal) }
    writeLines(terminal, 1...100)
    write(terminal, "\r\n\r\n\r\n\r\n\r\n")
    writeLines(terminal, 101...123)
    write(terminal, "line 124")

    let rows = try history(terminal, 10)
    #expect(rows.count == 10)
    #expect(Array(rows[..<5]) == numbered(96...100))
    #expect(Array(rows[5...]) == ["", "", "", "", ""])
    #expect(firstViewportRow(terminal) == "line 101")
}

@Test @MainActor
func blankRowsInsideHistoryKeepTheirPositions() throws {
    let terminal = try makeTerminal()
    defer { ghostty_terminal_free(terminal) }
    writeLines(terminal, 1...60)
    write(terminal, "\r\n\r\n\r\n")
    writeLines(terminal, 61...99)
    write(terminal, "line 100")

    let expected = numbered(1...60) + ["", "", ""] + numbered(61...76)
    let rows = try history(terminal, 10_000)
    let length = try historyLength(terminal)
    #expect(UInt32(rows.count) == length)
    #expect(rows == expected)
}

@Test @MainActor
func anAllBlankHistoryIsEmptyStringsNotAnEmptyArray() throws {
    let terminal = try makeTerminal()
    defer { ghostty_terminal_free(terminal) }
    write(terminal, String(repeating: "\r\n", count: 30))

    let length = try historyLength(terminal)
    #expect(length > 0, "30 blank rows scroll some of them out of view")
    let all = try history(terminal, length)
    let three = try history(terminal, 3)
    #expect(all == [String](repeating: "", count: Int(length)))
    #expect(three == ["", "", ""])
}

/// libghostty's own trim treats any cell whose base codepoint is a space
/// as blank, so it would drop this cell and its mark. The reader asks for
/// no trim and removes bare spaces itself, which keeps the mark and keeps
/// a dumped row equal to a copied one.
@Test @MainActor
func aTrailingSpaceCarryingACombiningMarkSurvives() throws {
    let terminal = try makeTerminal()
    defer { ghostty_terminal_free(terminal) }
    write(terminal, "abc \u{0301}\r\n")
    writeLines(terminal, 1...40)

    let rows = try history(terminal, 10_000)
    #expect(rows.first == "abc \u{0301}")
}

@Test @MainActor
func aScrolledViewportMovesTheHistoryAnchorWithIt() throws {
    let terminal = try makeTerminal()
    defer { ghostty_terminal_free(terminal) }
    writeLines(terminal, 1...100)
    scrollViewport(terminal, delta: -40)

    let length = try historyLength(terminal)
    #expect(length == 37)
    #expect(firstViewportRow(terminal) == "line 38")

    let rows = try history(terminal, 10)
    #expect(rows.count == 10)
    #expect(rows == numbered(28...37))
}

@Test @MainActor
func scrolledToTheVeryTopThereIsNoHistoryLeft() throws {
    let terminal = try makeTerminal()
    defer { ghostty_terminal_free(terminal) }
    writeLines(terminal, 1...100)
    scrollViewportToTop(terminal)

    let length = try historyLength(terminal)
    let rows = try history(terminal, 50)
    #expect(length == 0)
    #expect(rows == [])
    #expect(firstViewportRow(terminal) == "line 1")
}
