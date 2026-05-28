// Unit tests for `TerminalView.colRange` — the static helper that
// translates a multi-row selection into per-row `[startCol, endCol)`
// extents. Pure function, no libghostty needed.
//
// Same shape as the existing `RenderResolverTests` pattern: small,
// focused, isolated. The `colRange` function is the bit of selection
// rendering that we can unit-test without spinning up a real terminal;
// the screen-y ↔ viewport-row conversion path is exercised by the
// roost-vt FFI integration tests (`crates/roost-vt/tests/grid_ref_test.rs`).

import Testing

@testable import Roost

@Suite("TerminalView.colRange")
struct TerminalViewColRangeTests {
    private let cols = 80

    @Test func singleRowUsesLiteralCols() {
        let n: (startY: UInt32, startCol: Int, endY: UInt32, endCol: Int) =
            (10, 3, 11, 17)
        let (s, e) = TerminalView.colRange(
            forOffset: 0, totalRowSpan: 1, normalized: n, cols: cols
        )
        #expect(s == 3)
        #expect(e == 17)
    }

    @Test func firstRowOfMultiRowFillsToRightEdge() {
        let n: (startY: UInt32, startCol: Int, endY: UInt32, endCol: Int) =
            (10, 12, 14, 5)
        let (s, e) = TerminalView.colRange(
            forOffset: 0, totalRowSpan: 4, normalized: n, cols: cols
        )
        #expect(s == 12)
        #expect(e == cols)
    }

    @Test func interiorRowSpansFullWidth() {
        let n: (startY: UInt32, startCol: Int, endY: UInt32, endCol: Int) =
            (10, 12, 14, 5)
        let (s, e) = TerminalView.colRange(
            forOffset: 1, totalRowSpan: 4, normalized: n, cols: cols
        )
        #expect(s == 0)
        #expect(e == cols)
    }

    @Test func lastRowEndsAtEndCol() {
        let n: (startY: UInt32, startCol: Int, endY: UInt32, endCol: Int) =
            (10, 12, 14, 5)
        let (s, e) = TerminalView.colRange(
            forOffset: 3, totalRowSpan: 4, normalized: n, cols: cols
        )
        #expect(s == 0)
        #expect(e == 5)
    }
}
