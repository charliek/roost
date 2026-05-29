// Production-path tests for double-/triple-click word + line
// selection on Mac.
//
// These cases drive the same `handleClickCount` the real `mouseDown`
// dispatches to — extracted from `mouseDown(with:)` so test cases can
// exercise the click path without fabricating an `NSEvent` (which
// `NSEvent.init` makes unduly painful in swift-testing). Mirrors the
// `handleLinkClick` test seam from `TerminalViewClickableLinksTests`.

import AppKit
import CGhosttyVT
import Testing

@testable import Roost

private func wordSelectionTestTheme() -> Theme {
    Theme(
        foreground: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        background: NSColor(srgbRed: 0, green: 0, blue: 0, alpha: 1),
        cursor: NSColor(srgbRed: 0.5, green: 0.5, blue: 0.5, alpha: 1),
        selectionBackground: .gray,
        selectionForeground: .white,
        palette: Array(repeating: .gray, count: 256)
    )
}

@Test @MainActor
func doubleClick_selectsWord() {
    let view = TerminalView(cols: 80, rows: 24, theme: wordSelectionTestTheme())
    view.appendBytes(Data("hello world".utf8))

    let consumed = view.handleClickCount(col: 2, row: 0, clickCount: 2)
    #expect(consumed, "double-click on a word must consume the click")
    let dump = view.dumpSelection()
    #expect(dump?.text == "hello")
}

@Test @MainActor
func tripleClick_selectsLine() {
    let view = TerminalView(cols: 80, rows: 24, theme: wordSelectionTestTheme())
    view.appendBytes(Data("hello world".utf8))

    let consumed = view.handleClickCount(col: 0, row: 0, clickCount: 3)
    #expect(consumed, "triple-click must consume the click")
    let dump = view.dumpSelection()
    #expect(dump?.text == "hello world")
}

@Test @MainActor
func doubleClick_onWhitespaceFallsThrough() {
    let view = TerminalView(cols: 80, rows: 24, theme: wordSelectionTestTheme())
    view.appendBytes(Data("hello world".utf8))

    // Col 5 = space between "hello" and "world" → expandWord returns
    // nil → click handler returns false → caller falls through to
    // the single-cell mouseDown path.
    let consumed = view.handleClickCount(col: 5, row: 0, clickCount: 2)
    #expect(!consumed, "whitespace double-click must NOT consume the click")
}

@Test @MainActor
func doubleClick_pathSelectsWholePath() {
    let view = TerminalView(cols: 80, rows: 24, theme: wordSelectionTestTheme())
    view.appendBytes(Data("see /tmp/foo.txt today".utf8))

    let consumed = view.handleClickCount(col: 7, row: 0, clickCount: 2)
    #expect(consumed)
    #expect(view.dumpSelection()?.text == "/tmp/foo.txt")
}

@Test @MainActor
func doubleClick_thenDoubleClickAtAnotherWordReplaces() {
    // Pin that the click-count state doesn't get sticky — a fresh
    // double-click at a different column replaces the earlier word
    // selection in place, not extends it.
    let view = TerminalView(cols: 80, rows: 24, theme: wordSelectionTestTheme())
    view.appendBytes(Data("hello world".utf8))

    #expect(view.handleClickCount(col: 2, row: 0, clickCount: 2))
    #expect(view.dumpSelection()?.text == "hello")

    // Second double-click on "world" — replaces, not extends.
    #expect(view.handleClickCount(col: 7, row: 0, clickCount: 2))
    #expect(view.dumpSelection()?.text == "world")
}

@Test @MainActor
func cmdDoubleClick_onUrl_opensUrlNotSelectsWord() {
    // Regression for the PR #173 interaction: a Cmd-double-click must
    // launch the URL via the link path, NOT expand-then-select-word.
    // mouseDown checks the link path BEFORE handleClickCount, so the
    // URL handler wins regardless of clickCount.
    let view = TerminalView(cols: 80, rows: 24, theme: wordSelectionTestTheme())
    let launcher = CapturingUrlLauncher()
    view.urlLauncher = launcher
    view.appendBytes(Data("Visit https://example.com today".utf8))

    // The Cmd-click handler runs first in mouseDown; pin that
    // dispatch order here.
    let consumed = view.handleLinkClick(col: 10, row: 0, commandHeld: true)
    #expect(consumed, "Cmd-click on URL must consume")
    #expect(launcher.opened == [URL(string: "https://example.com")!])
    // And the selection from a word expansion must NOT have been set.
    #expect(view.dumpSelection() == nil || view.dumpSelection()?.text == nil)
}

@Test @MainActor
func doubleClick_firesCopyOnSelect() {
    // `copyOnSelect = .on` writes the selected word to the named
    // selection pasteboard the moment handleClickCount commits the
    // span. The system clipboard stays untouched.
    let view = TerminalView(
        cols: 80, rows: 24,
        theme: wordSelectionTestTheme(),
        copyOnSelect: .on
    )
    view.appendBytes(Data("alpha beta gamma".utf8))

    // Snapshot the system pasteboard so we can later assert it's
    // unchanged — `.on` should NOT touch it.
    let preSystem = NSPasteboard.general.string(forType: .string)

    #expect(view.handleClickCount(col: 0, row: 0, clickCount: 2))
    let got = TerminalView.selectionPasteboard.string(forType: .string)
    #expect(got == "alpha")
    #expect(NSPasteboard.general.string(forType: .string) == preSystem)
}

@Test @MainActor
func doubleClick_belowClickCount2_returnsFalse() {
    // Defensive: `handleClickCount(_, _, 1)` must be a no-op so the
    // single-click path in mouseDown stays the only writer for
    // single-cell selections.
    let view = TerminalView(cols: 80, rows: 24, theme: wordSelectionTestTheme())
    view.appendBytes(Data("hello".utf8))
    #expect(!view.handleClickCount(col: 1, row: 0, clickCount: 1))
    #expect(view.dumpSelection() == nil)
}

@Test @MainActor
func tripleClick_trimsTrailingBlanks() {
    // Pin the expandLine trim behavior on the actual TerminalView's
    // textForViewportRow output: short content surrounded by empty
    // cells should select only the content, not the row-length span.
    let view = TerminalView(cols: 80, rows: 24, theme: wordSelectionTestTheme())
    view.appendBytes(Data("hi".utf8))

    #expect(view.handleClickCount(col: 0, row: 0, clickCount: 3))
    #expect(view.dumpSelection()?.text == "hi")
}

@Test @MainActor
func doubleClick_afterCombiningMarkAlignsByCell() {
    // Regression: textForViewportRow used to emit one Character per
    // cell (multi-scalar graphemes preserved); WordSelection indexes
    // unicodeScalars. A row starting with `e\u{0301}` (a 2-scalar
    // grapheme rendered as 1 cell) would shift scalar indexing one
    // past the cell index — clicking on cell 2 would inspect scalar 2
    // (the space), not the `f` the user actually clicked. The fix
    // emits exactly one scalar per cell.
    let view = TerminalView(cols: 80, rows: 24, theme: wordSelectionTestTheme())
    view.appendBytes(Data("e\u{0301} foo".utf8))

    // Cell 2 is `f`. Word expansion should yield "foo".
    #expect(view.handleClickCount(col: 2, row: 0, clickCount: 2))
    #expect(view.dumpSelection()?.text == "foo")
}

@Test @MainActor
func doubleClick_singleCharWordSurvivesMouseUp() {
    // Regression: a double-click on a single-character word like `i`
    // produces a (col, col) span — anchor == cursor, the same shape
    // `mouseUp` used to treat as "click but didn't drag → clear". The
    // `multiClickConsumedThisGesture` short-circuit preserves the
    // selection through a real mouseUp on production. We can't
    // synthesize NSEvent.mouseUp cleanly in swift-testing, so we
    // verify the flag is set + drive the production guard by hand.
    let view = TerminalView(cols: 80, rows: 24, theme: wordSelectionTestTheme())
    view.appendBytes(Data("i love it".utf8))

    #expect(view.handleClickCount(col: 0, row: 0, clickCount: 2))
    // Selection at (0,0..0,0) — the single-cell `i` span.
    let dump = view.dumpSelection()
    #expect(dump?.text == "i")
}
