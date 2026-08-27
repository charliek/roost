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

// MARK: - Scrollback eviction (issue #334)

// Endpoints are tracked grid refs, so eviction has exactly two outcomes
// and no third: while the selected row survives the selection follows
// it, and once the row itself is pruned the selection resolves to
// nothing. It never lands on whatever content inherited the old
// coordinate — which is what stored screen-y endpoints used to do.
//
// The setup numbers are load-bearing. libghostty prunes at *page*
// granularity, so a scrollback limit smaller than one page evicts
// nothing until a whole page has filled and then drops the lot; to get
// a moment where older rows are gone and the selected row is not, the
// limit has to span several pages. Rows per page scale inversely with
// the column count (a page's byte budget comes from ghostty's standard
// capacity; the OS page size only rounds the allocation), so these
// tests run wide — 80 columns is ~594 rows/page on every target, and
// `TerminalView.defaultScrollback` (2000) therefore covers 3-4 pages.
// Rust twins in `crates/roost-vt/tests/selection_test.rs` use the same
// numbers.

/// Columns for the eviction tests. Wide on purpose (see above).
private let evictCols = 80
/// Filler written before the marker, so the marker is not in the very
/// first page — the page that gets pruned first.
private let evictPre = 1000
/// Rows written after the marker for the "older rows pruned" case.
private let evictSurvives = 1300
/// Rows written after the marker for the "selected row pruned" case.
private let evictPruned = 3000

@MainActor
private func makeEvictionView() -> TerminalView {
    TerminalView(
        cols: UInt16(evictCols),
        rows: UInt16(selectionCopyRows),
        theme: selectionCopyTheme()
    )
}

/// Fill history, then put `MARKER` on the bottom row and select it.
@MainActor
private func anchorMarker(_ view: TerminalView) {
    for i in 0..<evictPre { write(view, "filler\(i)\r\n") }
    write(view, "MARKER")
    #expect(view.setSelection(
        anchorCol: 0, anchorRow: selectionCopyRows - 1,
        cursorCol: evictCols - 1, cursorRow: selectionCopyRows - 1
    ))
}

/// Assert history really was pruned, so a green test cannot mean "the
/// scenario never happened": libghostty is holding fewer rows than were
/// written.
@MainActor
private func expectHistoryPruned(_ view: TerminalView, written: Int) {
    let total = view.totalScrollableRowsForTest()
    #expect(
        total < UInt64(written),
        "no history was pruned (\(total) rows held, \(written) written); the eviction scenario did not run"
    )
}

/// Scenario 1: rows *older* than the selection are pruned. The tracked
/// endpoints move with their content, so the copy is unchanged.
@Test @MainActor
func selectionFollowsItsContentWhenOlderHistoryIsPruned() {
    let view = makeEvictionView()
    anchorMarker(view)
    #expect(copied(view) == "MARKER")

    write(view, "\r\n")
    writeLines(view, evictSurvives)
    expectHistoryPruned(view, written: evictPre + 1 + evictSurvives)

    #expect(
        copied(view) == "MARKER",
        "selection drifted off its content instead of following it"
    )
}

/// Scenario 2: the selected row itself is pruned. Tracked refs cannot
/// follow discarded content, so the selection reports nothing — never
/// some other row's text.
@Test @MainActor
func selectionReportsNothingOnceItsOwnRowIsPruned() {
    let view = makeEvictionView()
    anchorMarker(view)

    write(view, "\r\n")
    writeLines(view, evictPruned)
    expectHistoryPruned(view, written: evictPre + 1 + evictPruned)

    #expect(copied(view) == nil, "a pruned selection resolved to text")
    let dump = view.dumpSelection()
    #expect(dump != nil, "the selection is still held, it just resolves to nothing")
    #expect(dump?.text == nil)
    #expect(dump?.anchorVisible == false)
    #expect(dump?.cursorVisible == false)
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
    // Soft-wrapped content: the rows the terminal broke for itself.
    assertPathsAgree(["abcdefghijklmnopqrstuvwxyz0123", "bravo", "charlie"])
    assertPathsAgree(["alpha", "abcdefghijklmnopqrstuvwxyz0123", "charlie"])
    assertPathsAgree(["alpha", "bravo", "abcdefghijklmnopqrstuvwxyz0123"])
    assertPathsAgree([
        "alpha        ", "bravo",
        "\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}\u{4f60}\u{597d}abc",
    ])
}

// MARK: - Soft-wrap unwrapping (plan 024 D4.4)

/// Write `input`, select `anchor...cursor`, and require the same text
/// both while the selection is visible (the render-state walk) and once
/// it has been pushed into scrollback (libghostty's formatter).
///
/// `joined` is what the copy should be with
/// `SelectionFormatter.unwrapSoftWrappedLines` on, `perRow` with it off,
/// so every case pins the behavior either way the constant is set.
@MainActor
private func assertWrappedCopy(
    _ input: String,
    anchor: (col: Int, row: Int),
    cursor: (col: Int, row: Int),
    joined: String,
    perRow: String
) {
    let expected = SelectionFormatter.unwrapSoftWrappedLines ? joined : perRow
    let view = makeView()
    write(view, input)
    #expect(view.setSelection(
        anchorCol: anchor.col, anchorRow: anchor.row,
        cursorCol: cursor.col, cursorRow: cursor.row
    ))
    #expect(copied(view) == expected, "visible copy of \(input.debugDescription)")

    scrollOut(view, 30)
    #expect(copied(view) == expected, "scrollback copy of \(input.debugDescription)")
}

/// 30 narrow cells at 20 columns: rows 0 and 1 are one logical line.
private let wrap2 = "abcdefghijklmnopqrstuvwxyz0123"
/// 45 narrow cells: rows 0, 1 and 2 are one logical line.
private let wrap3 = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHI"

@Test @MainActor
func aLineWrappedAcrossTwoRowsCopiesAsOneLine() {
    assertWrappedCopy(
        wrap2,
        anchor: (0, 0), cursor: (selectionCopyCols - 1, 1),
        joined: "abcdefghijklmnopqrstuvwxyz0123",
        perRow: "abcdefghijklmnopqrst\nuvwxyz0123"
    )
}

@Test @MainActor
func aLineWrappedAcrossThreeRowsCopiesAsOneLine() {
    assertWrappedCopy(
        wrap3,
        anchor: (0, 0), cursor: (selectionCopyCols - 1, 2),
        joined: "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHI",
        perRow: "abcdefghijklmnopqrst\nuvwxyz0123456789ABCD\nEFGHI"
    )
}

/// The wrap lands inside a word, which is the normal case — a terminal
/// breaks on the column, not on whitespace. Rejoining has to put the
/// word back together with nothing inserted between the halves.
@Test @MainActor
func aWrapInsideAWordRejoinsTheWord() {
    assertWrappedCopy(
        "wrapped-word-boundary-check",
        anchor: (0, 0), cursor: (selectionCopyCols - 1, 1),
        joined: "wrapped-word-boundary-check",
        perRow: "wrapped-word-boundar\ny-check"
    )
}

/// A real newline after a soft-wrapped line still breaks the copy. Only
/// the wrap is absorbed.
@Test @MainActor
func aHardNewlineAfterAWrappedLineSurvives() {
    assertWrappedCopy(
        "abcdefghijklmnopqrstuvwxyz0123\r\ntail",
        anchor: (0, 0), cursor: (selectionCopyCols - 1, 2),
        joined: "abcdefghijklmnopqrstuvwxyz0123\ntail",
        perRow: "abcdefghijklmnopqrst\nuvwxyz0123\ntail"
    )
}

/// Starting the selection part-way through a wrapped line keeps the rest
/// of that line on one line rather than breaking it at the screen edge.
@Test @MainActor
func aSelectionStartingMidWrappedLineStillJoinsTheRest() {
    assertWrappedCopy(
        wrap2,
        anchor: (5, 0), cursor: (selectionCopyCols - 1, 1),
        joined: "fghijklmnopqrstuvwxyz0123",
        perRow: "fghijklmnopqrst\nuvwxyz0123"
    )
}

/// A row of nothing but wide glyphs fills all 20 columns exactly and
/// wraps into the next row.
@Test @MainActor
func aWrappedLineOfWideGlyphsCopiesAsOneLine() {
    let cjk = String(repeating: "\u{4f60}\u{597d}", count: 5)
    assertWrappedCopy(
        cjk + "abc",
        anchor: (0, 0), cursor: (selectionCopyCols - 1, 1),
        joined: cjk + "abc",
        perRow: cjk + "\nabc"
    )
}

/// A wide grapheme that does not fit in the last column wraps whole,
/// leaving a placeholder behind. The rejoined line must carry the
/// grapheme exactly once and put nothing where the placeholder was.
@Test @MainActor
func aWideGraphemeStraddlingTheWrapBoundaryCopiesOnce() {
    assertWrappedCopy(
        "1234567890123456789\u{754c}XY",
        anchor: (0, 0), cursor: (3, 1),
        joined: "1234567890123456789\u{754c}XY",
        perRow: "1234567890123456789\n\u{754c}XY"
    )
}

/// Ending the selection ON that placeholder is the case libghostty
/// handles by reaching into the next row for the grapheme that wrapped
/// (`PageFormatter.formatWithState`'s spacer-head adjustment). The
/// viewport walk cannot mirror that reach — the limit is page-relative —
/// so it hands the selection back to the formatter, which is why the two
/// scroll positions still agree.
@Test @MainActor
func aSelectionEndingOnAWrappedWidePlaceholderPicksUpTheGrapheme() {
    assertWrappedCopy(
        "1234567890123456789\u{754c}XY",
        anchor: (0, 0), cursor: (selectionCopyCols - 1, 0),
        joined: "1234567890123456789\u{754c}",
        perRow: "1234567890123456789"
    )
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
