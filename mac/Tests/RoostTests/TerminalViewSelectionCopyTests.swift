// Selection-copy tests for the Mac UI (issue #249).
//
// Copy used to be viewport-clipped: `selectedPlainText` mapped the
// selection's screen rows onto currently-visible viewport rows and
// dropped everything else, so a selection reaching into scrollback came
// back truncated. It now routes through libghostty's selection-scoped
// formatter (`SelectionFormatter`) whenever an endpoint is off screen,
// and the render-state walk that still handles fully-visible selections
// was aligned with the formatter so the two cannot disagree.
//
// Swift mirror of `crates/roost-vt/tests/selection_test.rs`; the cases
// are deliberately the same ones so a divergence between the Mac and
// Rust UIs shows up as a failing test on one side.
//
// The selection is driven through `setSelection` (the same entry point
// the `selection.set` IPC op and the multi-click dispatch use) and read
// back through `dumpSelection`, which shares `selectedPlainText` with
// ⌘C and copy-on-select.

import AppKit
import Testing

@testable import Roost

private let selectionCopyCols = 20
private let selectionCopyRows = 5

private func selectionCopyTheme() -> Theme {
    Theme(
        foreground: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        background: NSColor(srgbRed: 0, green: 0, blue: 0, alpha: 1),
        cursor: NSColor(srgbRed: 0.5, green: 0.5, blue: 0.5, alpha: 1),
        selectionBackground: .gray,
        selectionForeground: .white,
        palette: Array(repeating: .gray, count: 256)
    )
}

@MainActor
private func makeView() -> TerminalView {
    TerminalView(
        cols: UInt16(selectionCopyCols),
        rows: UInt16(selectionCopyRows),
        theme: selectionCopyTheme()
    )
}

@MainActor
private func write(_ view: TerminalView, _ text: String) {
    view.appendBytes(Data(text.utf8))
}

@MainActor
private func writeLines(_ view: TerminalView, _ count: Int) {
    for i in 0..<count { write(view, "line\(i)\r\n") }
}

/// Push whatever is on screen up into scrollback, starting on a fresh
/// row so the filler never lands on the selected content.
@MainActor
private func scrollOut(_ view: TerminalView, _ count: Int) {
    write(view, "\r\n")
    writeLines(view, count)
}

@MainActor
private func copied(_ view: TerminalView) -> String? {
    view.dumpSelection()?.text
}

// MARK: - Scrollback-spanning copy

/// **Acceptance criterion W-D(2).** With the fix reverted this fails:
/// the old walk mapped only visible viewport rows, so once the whole
/// selection had scrolled into history it returned nil.
@Test @MainActor
func selectionScrolledEntirelyAboveTheViewportIsNotClipped() {
    let view = makeView()
    write(view, "alpha\r\nbravo\r\ncharlie")
    #expect(view.setSelection(anchorCol: 0, anchorRow: 0, cursorCol: 6, cursorRow: 2))
    #expect(copied(view) == "alpha\nbravo\ncharlie")

    scrollOut(view, 30)
    #expect(copied(view) == "alpha\nbravo\ncharlie")
}

@Test @MainActor
func selectionStraddlingTheTopEdgeKeepsItsScrolledOffRows() {
    let view = makeView()
    write(view, "alpha\r\nbravo\r\ncharlie\r\ndelta")
    #expect(view.setSelection(anchorCol: 0, anchorRow: 0, cursorCol: 4, cursorRow: 3))

    // Two rows scroll off; the rest stay visible.
    scrollOut(view, 2)
    #expect(copied(view) == "alpha\nbravo\ncharlie\ndelta")
}

/// A tall selection buried deep in history: every row has to come
/// back, not just the handful the viewport happens to show.
@Test @MainActor
func selectionOfManyRowsSurvivesBeingPushedDeepIntoScrollback() {
    let tallRows = 50
    let view = TerminalView(
        cols: UInt16(selectionCopyCols), rows: UInt16(tallRows), theme: selectionCopyTheme()
    )
    for i in 0..<tallRows {
        write(view, i == tallRows - 1 ? "line\(i)" : "line\(i)\r\n")
    }
    #expect(view.setSelection(
        anchorCol: 0, anchorRow: 0,
        cursorCol: selectionCopyCols - 1, cursorRow: tallRows - 1
    ))
    scrollOut(view, 200)

    let text = copied(view)
    let lines = (text ?? "").components(separatedBy: "\n")
    #expect(lines.first == "line0")
    #expect(lines.last == "line\(tallRows - 1)")
    #expect(lines.count == tallRows, "clipped: \(String(describing: text))")
}

@Test @MainActor
func reversedDragIntoScrollbackCopiesInDocumentOrder() {
    let view = makeView()
    write(view, "alpha\r\nbravo\r\ncharlie")
    // Anchor below, cursor above: libghostty orders the endpoints.
    #expect(view.setSelection(anchorCol: 6, anchorRow: 2, cursorCol: 0, cursorRow: 0))
    scrollOut(view, 30)
    #expect(copied(view) == "alpha\nbravo\ncharlie")
}

@Test @MainActor
func scrollbackCopyPreservesWideAndCombiningGlyphs() {
    let view = makeView()
    write(view, "\u{4f60}\u{597d}\r\na\u{301}bc")
    #expect(view.setSelection(
        anchorCol: 0, anchorRow: 0, cursorCol: selectionCopyCols - 1, cursorRow: 1
    ))
    scrollOut(view, 30)
    #expect(copied(view) == "\u{4f60}\u{597d}\na\u{301}bc")
}

@Test @MainActor
func scrollbackCopyKeepsInteriorBlankRows() {
    let view = makeView()
    write(view, "alpha\r\n\r\nbravo")
    #expect(view.setSelection(
        anchorCol: 0, anchorRow: 0, cursorCol: selectionCopyCols - 1, cursorRow: 2
    ))
    scrollOut(view, 30)
    #expect(copied(view) == "alpha\n\nbravo")
}

// MARK: - Viewport path vs formatter path

/// Copy the same selection twice — once while it is fully visible (the
/// render-state walk) and once after it has been pushed into scrollback
/// (the formatter) — and require the two to agree. A fast path that
/// disagreed with the slow one would make a copy depend on scroll
/// position.
@MainActor
private func assertPathsAgree(_ lines: [String]) {
    let view = makeView()
    writeLines(view, 40)
    write(view, lines.joined(separator: "\r\n"))

    #expect(view.setSelection(
        anchorCol: 0, anchorRow: 2,
        cursorCol: selectionCopyCols - 1, cursorRow: selectionCopyRows - 1
    ))
    let visible = copied(view)

    scrollOut(view, 30)
    let scrolled = copied(view)

    #expect(
        visible == scrolled,
        "paths disagree for \(lines): visible=\(String(describing: visible)) scrolled=\(String(describing: scrolled))"
    )
}

@Test @MainActor
func bothPathsAgree() {
    assertPathsAgree(["alpha", "bravo", "charlie"])
    assertPathsAgree(["alpha", "", "bravo"])
    assertPathsAgree(["alpha", "bravo", ""])
    assertPathsAgree(["alpha", "   ", "bravo"])
    assertPathsAgree(["alpha   ", "bravo", "charlie"])
    assertPathsAgree(["a\tb", "c\td", "ef"])
    assertPathsAgree(["a\u{301}bc", "bravo", "charlie"])
    assertPathsAgree(["", "", ""])
    // Cases that used to diverge.
    assertPathsAgree(["\u{4f60}\u{597d}", "bravo", "charlie"])
    assertPathsAgree(["a\u{754c}b", "c\u{754c}", "\u{754c}d"])
    assertPathsAgree(["", "alpha", "bravo"])
    assertPathsAgree(["", "", "alpha"])
    assertPathsAgree(["   ", "alpha", "bravo"])
    assertPathsAgree(["alpha", "bravo", "   "])
    assertPathsAgree(["   ", "   ", "   "])
    // A wide glyph that cannot fit in the last column wraps, leaving a
    // spacer head behind on the row it did not fit on.
    assertPathsAgree(["1234567890123456789\u{754c}", "bravo", "charlie"])
}

/// Write `input` into a fresh view, select all of it, and require
/// `expected` both while it is visible and once it is in scrollback.
@MainActor
private func assertCopiesAs(_ input: String, _ expected: String) {
    let view = makeView()
    write(view, input)
    let lastRow = input.components(separatedBy: "\r\n").count - 1
    #expect(view.setSelection(
        anchorCol: 0, anchorRow: 0,
        cursorCol: selectionCopyCols - 1, cursorRow: lastRow
    ))
    #expect(
        copied(view) == expected,
        "visible copy of \(input.debugDescription)"
    )

    scrollOut(view, 30)
    #expect(
        copied(view) == expected,
        "scrollback copy of \(input.debugDescription)"
    )
}

/// The cases the viewport walk used to get wrong. Asserted as exact
/// values in both scroll positions so the behavior itself — not just
/// the agreement — is pinned.
@Test @MainActor
func previouslyDivergentCasesNowMatchTheFormatter() {
    // 1. Wide glyphs: no phantom space for the spacer cell.
    assertCopiesAs("\u{4f60}\u{597d}", "\u{4f60}\u{597d}")
    // 2. Leading blank rows are preserved.
    assertCopiesAs("\r\nalpha", "\nalpha")
    // 3. A trailing row of only spaces still ends the previous line.
    assertCopiesAs("alpha\r\n   ", "alpha\n")
    // 4. A space carrying a combining mark keeps the mark. libghostty's
    //    own `trim` would drop it (it treats the cell as blank and
    //    re-emits a bare space), which is why the formatter is asked
    //    for untrimmed output.
    assertCopiesAs("a \u{301}b", "a \u{301}b")
}

/// A selection whose start column lands on a wide grapheme's
/// placeholder reaches back to the grapheme itself; one that starts on
/// the placeholder left behind by a grapheme that wrapped skips that
/// row entirely. Both rules come from the formatter, and the viewport
/// walk mirrors them.
@Test @MainActor
func selectionStartingOnAWidePlaceholderMatchesTheFormatter() {
    let tail = makeView()
    write(tail, "ab\u{754c}cd")
    #expect(tail.setSelection(anchorCol: 3, anchorRow: 0, cursorCol: 5, cursorRow: 0))
    #expect(copied(tail) == "\u{754c}cd")
    scrollOut(tail, 30)
    #expect(copied(tail) == "\u{754c}cd")

    // 19 narrow cells then a wide grapheme that cannot fit: column 19
    // is left as a placeholder and the grapheme wraps.
    let head = makeView()
    write(head, "1234567890123456789\u{754c}")
    #expect(head.setSelection(
        anchorCol: selectionCopyCols - 1, anchorRow: 0, cursorCol: 1, cursorRow: 1
    ))
    #expect(copied(head) == "\u{754c}")
    scrollOut(head, 30)
    #expect(copied(head) == "\u{754c}")
}
